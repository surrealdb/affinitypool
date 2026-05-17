//! Fast-path completion primitive for [`Threadpool::spawn`].
//!
//! This module provides a single-allocation alternative to
//! `tokio::sync::oneshot` for the common `spawn` flow. The completion struct
//! is shared between the worker (producer) and the awaiter (consumer) via an
//! `Arc`, with a small atomic state machine driving readiness and an
//! [`AtomicWaker`] driving wake-ups.
//!
//! # State machine
//!
//! `state` transitions monotonically through three values:
//!
//! ```text
//!     EMPTY  --(worker writes result)-->  READY  --(future reads result)-->  TAKEN
//! ```
//!
//! Reads of `state` use `Acquire` on the consumer side; writes use `Release`
//! on the producer side. This means that observing `READY` synchronises with
//! the worker's prior `MaybeUninit::write` of the result.
//!
//! # Safety
//!
//! `SpawnCompletion<R>: Send + Sync` is conditional on `R: Send`. The
//! producer's `Release`-store of `READY` happens-before the consumer's
//! `Acquire`-load that observes `READY`, so the read of the result through
//! `UnsafeCell` does not race with the producer's write.

use crate::atomic_waker::AtomicWaker;
use std::cell::UnsafeCell;
use std::future::Future;
use std::mem::MaybeUninit;
use std::panic;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::task::{Context, Poll};
use std::thread;

/// Empty: the worker has not yet written the result.
const EMPTY: u8 = 0;
/// Ready: the worker has written the result; the consumer may read it.
const READY: u8 = 1;
/// Taken: the consumer has moved the result out (or is dropping it). No
/// further reads are permitted.
const TAKEN: u8 = 2;

/// Shared completion slot between a worker and an awaiter.
///
/// `R` is the task's return type. The slot is occupied at most once: the
/// worker writes a `thread::Result<R>` (capturing panics) and then transitions
/// the state machine to `READY`; the consumer takes the value via
/// `MaybeUninit::assume_init_read` and transitions to `TAKEN`. The remaining
/// strong references' `Arc` drop is responsible for releasing any payload
/// that was written but never consumed (e.g. the awaiter was dropped between
/// the worker's `Release`-store and the consumer's poll).
pub(crate) struct SpawnCompletion<R> {
	state: AtomicU8,
	result: UnsafeCell<MaybeUninit<thread::Result<R>>>,
	waker: AtomicWaker,
}

// Safety: see module-level docs. The result moves between threads inside
// `MaybeUninit`, gated by the `state` machine, which is the minimal `Send`
// bound on `R`.
unsafe impl<R: Send> Send for SpawnCompletion<R> {}
unsafe impl<R: Send> Sync for SpawnCompletion<R> {}

impl<R> SpawnCompletion<R> {
	pub(crate) fn new() -> Self {
		Self {
			state: AtomicU8::new(EMPTY),
			result: UnsafeCell::new(MaybeUninit::uninit()),
			waker: AtomicWaker::new(),
		}
	}

	/// Worker-side: store the result and notify the awaiter.
	///
	/// This is called at most once per `SpawnCompletion`. If the state is
	/// already `TAKEN` (the awaiter dropped before we wrote), we still write
	/// — the value will be dropped when the last `Arc` strong reference dies.
	pub(crate) fn complete(&self, value: thread::Result<R>) {
		// Safety: the worker is the unique writer and runs at most once per
		// completion. The state can only have been moved out of EMPTY by
		// this method itself, so it is still EMPTY when we write.
		unsafe {
			(*self.result.get()).write(value);
		}
		// Release the write of `result` to anyone who observes `READY`.
		self.state.store(READY, Ordering::Release);
		self.waker.wake();
	}
}

impl<R> Drop for SpawnCompletion<R> {
	fn drop(&mut self) {
		// If a result was written but never read out, drop it now so we don't
		// leak the payload.
		if *self.state.get_mut() == READY {
			// Safety: state == READY means the worker performed the write,
			// and TAKEN was never reached, so the value is still initialised
			// and no other reference exists (we have `&mut self`).
			unsafe {
				(*self.result.get()).assume_init_drop();
			}
		}
	}
}

/// Future returned by [`crate::Threadpool::spawn`]. Polls the
/// [`SpawnCompletion`] state machine and propagates panics from the user
/// closure to the awaiter.
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub(crate) struct SpawnHandle<R> {
	completion: Option<Arc<SpawnCompletion<R>>>,
}

impl<R> SpawnHandle<R> {
	pub(crate) fn new(completion: Arc<SpawnCompletion<R>>) -> Self {
		Self {
			completion: Some(completion),
		}
	}

	/// Consume the ready result from the completion slot. Caller must have
	/// already observed `state == READY` under an `Acquire` load.
	fn take_ready(&mut self) -> Poll<R> {
		let completion = self.completion.as_ref().expect("already taken");
		// Safety: an Acquire-load of `READY` synchronises with the worker's
		// `Release`-store of the result, so the slot is fully initialised
		// and uniquely owned by this consumer (the future is single-poll
		// after Ready by contract).
		let value = unsafe { (*completion.result.get()).assume_init_read() };
		completion.state.store(TAKEN, Ordering::Release);
		// Drop our Arc handle so subsequent polls hit the `expect` above.
		self.completion = None;
		match value {
			Ok(v) => Poll::Ready(v),
			Err(payload) => panic::resume_unwind(payload),
		}
	}
}

impl<R> Future for SpawnHandle<R> {
	type Output = R;

	fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		// SpawnHandle has no pinned fields, so it's safe to unpin here.
		let this = Pin::into_inner(self);
		let completion = this.completion.as_ref().expect("SpawnHandle polled after completion");
		// Fast path: if the worker already wrote the result, skip the
		// waker-register CAS dance entirely.
		if completion.state.load(Ordering::Acquire) == READY {
			return this.take_ready();
		}
		// Slow path: register the waker, then re-check state to close the
		// race with a concurrent `complete()` between the load above and
		// the register call.
		completion.waker.register(cx.waker());
		if completion.state.load(Ordering::Acquire) == READY {
			return this.take_ready();
		}
		Poll::Pending
	}
}
