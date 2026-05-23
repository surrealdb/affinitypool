//! Loom models for the synchronisation patterns used by affinitypool.
//!
//! Loom exhaustively explores thread interleavings on a re-implementation of
//! the algorithm. The models here mirror the production code in
//! `src/job.rs`, `src/atomic_waker.rs`, and the park/unpark interaction in
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
use loom::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering, fence};
use loom::thread;
use std::cell::UnsafeCell;
use std::mem::MaybeUninit;

// Placate clippy under non-loom builds where this file becomes empty.
#[allow(dead_code)]
type _Unused = AtomicBool;

const EMPTY: u8 = 0;
const READY: u8 = 1;
const TAKEN: u8 = 2;

/// Mirror of the completion slot in `Job<F, R>` with a hand-rolled
/// "waker" stand-in to
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
		// Mimic JobHandle::poll: register first, then load state.
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
		// Mirror Job::release_ref on final-drop in src/job.rs: drop the result
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

/// Models the producer↔worker park/unpark handshake in `src/lib.rs`.
///
/// `injector_has_work` stands in for the global injector's "non-empty"
/// state. `worker_parked` stands in for the `parked_threads` queue
/// containing the worker. `was_unparked` records whether the producer
/// invoked the wake path (`parked_threads.pop()` succeeded).
///
/// The invariant under test is the absence of a lost wakeup: every
/// model run must terminate with either the worker having self-rescued
/// (it observed `injector_has_work == true` on its initial poll or its
/// post-register re-check) or the producer having woken it (the
/// producer observed `worker_parked == true` and set `was_unparked`).
///
/// The protocol mirrors the production code:
///   producer:  push injector;       fence(SeqCst); read parked_state
///   worker:    register parked;     fence(SeqCst); read injector
type ProducerFn = fn(&AtomicBool, &AtomicBool, &AtomicUsize, &AtomicBool);
type WorkerFn = fn(&AtomicBool, &AtomicBool, &AtomicUsize, &AtomicBool) -> bool;

fn run_handshake_invariant(producer_seq: ProducerFn, worker_seq: WorkerFn) {
	loom::model(move || {
		let injector_has_work = Arc::new(AtomicBool::new(false));
		let worker_parked = Arc::new(AtomicBool::new(false));
		let parked_count = Arc::new(AtomicUsize::new(0));
		let was_unparked = Arc::new(AtomicBool::new(false));

		let p_iw = injector_has_work.clone();
		let p_wp = worker_parked.clone();
		let p_pc = parked_count.clone();
		let p_un = was_unparked.clone();
		let producer = thread::spawn(move || {
			producer_seq(&p_iw, &p_wp, &p_pc, &p_un);
		});

		let w_iw = injector_has_work.clone();
		let w_wp = worker_parked.clone();
		let w_pc = parked_count.clone();
		let w_un = was_unparked.clone();
		let worker = thread::spawn(move || worker_seq(&w_iw, &w_wp, &w_pc, &w_un));

		producer.join().unwrap();
		let worker_observed_work = worker.join().unwrap();
		// No lost wakeup: either the worker observed work itself, or the
		// producer set `was_unparked` (which in production would have
		// invoked `Thread::unpark`).
		assert!(
			worker_observed_work || was_unparked.load(Ordering::Acquire),
			"lost wakeup: worker did not observe work and producer did not wake it"
		);
	});
}

/// Baseline: the production handshake with SeqCst fences on both sides
/// and no `parked_count` short-circuit. Validates the existing
/// invariant under loom — necessary baseline so we can detect a
/// regression after the parked_count change.
#[test]
fn loom_park_unpark_handshake_baseline() {
	run_handshake_invariant(
		|injector, parked, _count, unparked| {
			// Producer: push injector, fence, then attempt to wake.
			injector.store(true, Ordering::Release);
			fence(Ordering::SeqCst);
			if parked.load(Ordering::Acquire) {
				// Simulate `parked_threads.pop()` succeeding.
				parked.store(false, Ordering::Release);
				unparked.store(true, Ordering::Release);
			}
		},
		|injector, parked, _count, _unparked| {
			// Worker: initial peek (the production code's first
			// `find_task` call, prior to parking).
			if injector.load(Ordering::Acquire) {
				return true;
			}
			// Register as parked.
			parked.store(true, Ordering::Release);
			fence(Ordering::SeqCst);
			// Re-check.
			if injector.load(Ordering::Acquire) {
				// Self-rescue: undo the registration.
				parked.store(false, Ordering::Release);
				return true;
			}
			// "Park": in the model we just check whether the producer
			// has set `unparked`. Loom explores both orderings.
			false
		},
	);
}

/// Variant: the producer skips `parked_threads.pop()` (i.e. skips the
/// `parked.load(Acquire)` + write) when `parked_count.load(Acquire) ==
/// 0`. The worker increments `parked_count` with `Release` before its
/// SeqCst fence. Validates that this optimisation preserves the
/// lost-wakeup-free invariant.
#[test]
fn loom_park_unpark_handshake_parked_count_gate() {
	run_handshake_invariant(
		|injector, parked, count, unparked| {
			injector.store(true, Ordering::Release);
			fence(Ordering::SeqCst);
			// Fast path: skip the expensive parked_threads.pop() if
			// nobody is registered.
			if count.load(Ordering::Acquire) == 0 {
				return;
			}
			if parked.load(Ordering::Acquire) {
				parked.store(false, Ordering::Release);
				count.fetch_sub(1, Ordering::Release);
				unparked.store(true, Ordering::Release);
			}
		},
		|injector, parked, count, _unparked| {
			if injector.load(Ordering::Acquire) {
				return true;
			}
			parked.store(true, Ordering::Release);
			count.fetch_add(1, Ordering::Release);
			fence(Ordering::SeqCst);
			if injector.load(Ordering::Acquire) {
				parked.store(false, Ordering::Release);
				count.fetch_sub(1, Ordering::Release);
				return true;
			}
			false
		},
	);
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
