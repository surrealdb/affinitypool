//! `spawn_local` implementation built on top of [`async_task::Task`].
//!
//! `SpawnFuture` is a thin wrapper around `async_task::Task<R>` that
//! carries a `'pool` lifetime (to express the borrow of the
//! [`Threadpool`]) and runs a blocking cancellation in `Drop`.
//!
//! # Drop-blocking contract
//!
//! The closure passed to `spawn_local` may borrow producer-stack data
//! whose lifetime is `'pool`. To make the lifetime erasure required by
//! [`async_task::Builder::spawn_unchecked`] sound, dropping the future
//! while the worker may still be executing the closure must block the
//! dropping thread until the runnable has stopped. That is exactly the
//! semantic of `Task::cancel().await`; we run it on a parker-based
//! `block_on` because `Drop` is synchronous.
//!
//! # Leak amplification
//!
//! `mem::forget`-ing a `SpawnFuture` keeps the underlying `Task` alive
//! (its memory is leaked), but the runnable may still be in the pool
//! queue. The current implementation does not statically prevent the
//! borrow checker from accepting code that runs the closure after the
//! borrowed data has gone out of scope. This is the same soundness
//! profile as the previous implementation; closing it would require an
//! API change (a scoped spawn like `std::thread::scope`).

use async_task::Task;
use std::any::Any;
use std::future::Future;
use std::marker::PhantomData;
use std::panic;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread;

use crate::Threadpool;

/// The inner task type. The closure is wrapped in `catch_unwind` on
/// the worker side (see `Threadpool::spawn_local`), so the future's
/// output is a `Result` — `Ok(R)` if the closure returned normally,
/// `Err(payload)` if it panicked.
type Inner<R> = Task<Result<R, Box<dyn Any + Send + 'static>>>;

/// A future returned by [`Threadpool::spawn_local`].
///
/// Resolves to the closure's return value. Dropping the future before
/// completion cancels the task; if the worker is currently running the
/// closure, the dropping thread is parked until the worker has
/// finished. See module docs.
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct SpawnFuture<'pool, R> {
	/// `Some` until the future has resolved or been dropped.
	task: Option<Inner<R>>,
	/// Phantom borrow of the pool — ties the future's lifetime to the
	/// [`Threadpool`] reference it was created from, ensuring the pool
	/// outlives any in-flight tasks.
	_pool: PhantomData<&'pool Threadpool>,
}

impl<'pool, R> SpawnFuture<'pool, R> {
	#[inline]
	pub(crate) fn new(task: Inner<R>) -> Self {
		Self {
			task: Some(task),
			_pool: PhantomData,
		}
	}
}

impl<R> Future for SpawnFuture<'_, R> {
	type Output = R;

	fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		// Structural pinning: we never move `self.task` after creation.
		let this = unsafe { self.as_mut().get_unchecked_mut() };
		match this.task.as_mut() {
			Some(task) => match Pin::new(task).poll(cx) {
				Poll::Ready(result) => {
					// Task consumed itself; clear our slot so Drop
					// becomes a no-op.
					this.task = None;
					match result {
						Ok(value) => Poll::Ready(value),
						Err(payload) => panic::resume_unwind(payload),
					}
				}
				Poll::Pending => Poll::Pending,
			},
			None => panic!("SpawnFuture polled after completion"),
		}
	}
}

impl<R> Drop for SpawnFuture<'_, R> {
	fn drop(&mut self) {
		if let Some(task) = self.task.take() {
			// `Task::cancel()` returns a future that resolves once the
			// runnable has finished (either by running to completion
			// or by being dropped without running). Block the current
			// thread on that — the closure may borrow `'pool` data
			// that goes out of scope as soon as this returns.
			block_on_cancel(task);
		}
	}
}

// `SpawnFuture` is `Send` whenever the underlying `Task` is. async-task
// provides the bound automatically — re-state it here for clarity.
unsafe impl<R: Send> Send for SpawnFuture<'_, R> {}

/// Parker-based `block_on` for `task.cancel()`. Only used on the drop
/// path, so this is not a hot path — the contract is correctness, not
/// throughput.
fn block_on_cancel<R>(task: Inner<R>) {
	struct ParkWaker(thread::Thread);
	impl Wake for ParkWaker {
		fn wake(self: Arc<Self>) {
			self.0.unpark();
		}
		fn wake_by_ref(self: &Arc<Self>) {
			self.0.unpark();
		}
	}

	let waker: Waker = Arc::new(ParkWaker(thread::current())).into();
	let mut cx = Context::from_waker(&waker);
	let mut fut = task.cancel();
	// SAFETY: `fut` is owned by this function and never moved after
	// being pinned.
	let mut fut = unsafe { Pin::new_unchecked(&mut fut) };
	loop {
		match fut.as_mut().poll(&mut cx) {
			Poll::Ready(_) => return,
			Poll::Pending => thread::park(),
		}
	}
}
