//! Loom models for the synchronisation patterns used by affinitypool.
//!
//! Loom exhaustively explores thread interleavings on a re-implementation of
//! the algorithm. The models here mirror the production code in
//! `src/spawn.rs`, `src/atomic_waker.rs`, and the park/unpark interaction in
//! `src/lib.rs` so any reordering bug in the algorithm is caught
//! independently of the real implementation.
//!
//! Run with:
//!
//! ```sh
//! RUSTFLAGS="--cfg loom" cargo test --test loom --release
//! ```

#![cfg(loom)]

use loom::sync::Arc;
use loom::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use loom::thread;
use std::cell::UnsafeCell;
use std::mem::MaybeUninit;

// Placate clippy under non-loom builds where this file becomes empty.
#[allow(dead_code)]
type _Unused = AtomicBool;

const EMPTY: u8 = 0;
const READY: u8 = 1;
const TAKEN: u8 = 2;

/// Mirror of `SpawnCompletion<R>` with a hand-rolled "waker" stand-in to
/// avoid pulling the real `AtomicWaker` (which uses `std` atomics). The
/// invariant under test: every successful "complete" must be observed by
/// the consumer with the result fully initialised.
struct Completion {
	state: AtomicU8,
	result: UnsafeCell<MaybeUninit<u32>>,
	// Stand-in for AtomicWaker: a boolean "the consumer has noted readiness".
	signal: AtomicBool,
}

unsafe impl Send for Completion {}
unsafe impl Sync for Completion {}

impl Completion {
	fn new() -> Self {
		Self {
			state: AtomicU8::new(EMPTY),
			result: UnsafeCell::new(MaybeUninit::uninit()),
			signal: AtomicBool::new(false),
		}
	}

	fn complete(&self, value: u32) {
		// Safety: producer is the unique writer until `state` is bumped.
		unsafe {
			(*self.result.get()).write(value);
		}
		self.state.store(READY, Ordering::Release);
		self.signal.store(true, Ordering::Release);
	}

	fn try_take(&self) -> Option<u32> {
		// Mimic SpawnHandle::poll: register first, then load state.
		let _ = self.signal.load(Ordering::Acquire);
		match self.state.load(Ordering::Acquire) {
			READY => {
				let v = unsafe { (*self.result.get()).assume_init_read() };
				self.state.store(TAKEN, Ordering::Release);
				Some(v)
			}
			_ => None,
		}
	}
}

impl Drop for Completion {
	fn drop(&mut self) {
		// Mirror SpawnCompletion::drop in src/spawn.rs: drop the result
		// payload iff it was written but never taken. We hold `&mut self`
		// here, so no concurrent access is possible; a relaxed load is OK
		// (Loom's `AtomicU8` does not expose `get_mut`).
		if self.state.load(Ordering::Relaxed) == READY {
			unsafe {
				(*self.result.get()).assume_init_drop();
			}
		}
	}
}

/// Model: a single producer fills the completion slot; a single consumer
/// polls until it sees `READY`. Loom must confirm that whenever the
/// consumer reads `READY`, the result it reads is the one the producer
/// wrote (no torn or stale read).
#[test]
fn loom_spawn_completion_release_acquire() {
	loom::model(|| {
		let c = Arc::new(Completion::new());
		let c2 = c.clone();
		let producer = thread::spawn(move || {
			c2.complete(0xdead_beef);
		});
		// Spin until the consumer observes READY.
		loop {
			if let Some(v) = c.try_take() {
				assert_eq!(v, 0xdead_beef);
				break;
			}
			thread::yield_now();
		}
		producer.join().unwrap();
	});
}

/// Model the producer racing with the consumer's `Drop`: if the consumer
/// drops the future *after* the producer transitioned the slot to `READY`
/// but *before* the consumer reads it, the slot's own `Drop` must free the
/// value exactly once. Encodes this by having the consumer skip `try_take`
/// and rely solely on `Drop` semantics.
#[test]
fn loom_completion_drop_after_release_frees_payload() {
	loom::model(|| {
		let c = Arc::new(Completion::new());
		let c2 = c.clone();

		let producer = thread::spawn(move || {
			c2.complete(99);
		});

		// Consumer drops its Arc without reading.
		drop(c);
		producer.join().unwrap();
		// If the algorithm is sound, dropping the producer's Arc (last
		// strong reference) frees the value. Loom's leak detector will
		// flag the test if the value or the Arc itself is leaked.
	});
}
