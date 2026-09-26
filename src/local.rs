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
//! # Leak amplification and the `unsafe` contract
//!
//! The drop-blocking contract above is only load-bearing if the
//! destructor actually runs, and Rust never guarantees that. Safe code
//! can `mem::forget` a `SpawnFuture` (or leak it via `Box::leak`, a
//! `ManuallyDrop`, an `Rc`/`Arc` cycle, or a leaked enclosing future):
//! its `Task` stays alive but the runnable may still be queued or
//! running, and the closure can then read borrowed producer-stack data
//! after it has gone out of scope. That is a use-after-free reachable
//! from entirely safe code.
//!
//! This cannot be closed by an implementation change. A future is a
//! value, leaking a value is always safe, so a destructor can never be
//! a sound safety barrier in async code. The only leak-proof design is
//! a *synchronous* scoped API (`std::thread::scope` / `rayon::scope`),
//! where the join happens as the scope call returns — a barrier safe
//! code cannot skip. There is no `.await`-able equivalent.
//!
//! Because the hazard is intrinsic, [`Threadpool::spawn_local`] and the
//! free [`crate::spawn_local`] are `unsafe`: their `# Safety` contract
//! requires the caller never to leak the returned future while it
//! borrows non-`'static` data. Normal use (`.await`, or a plain drop)
//! upholds it automatically; `tests/unsound.rs` is the one documented
//! way it breaks.

use async_task::{Runnable, Task};
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
/// Resolves to the closure's return value. The runnable is scheduled
/// lazily on first poll — constructing a `SpawnFuture` and dropping it
/// without polling never touches a worker. Dropping after polling
/// cancels the task; if the worker is currently running the closure,
/// the dropping thread is parked until the worker has finished. See
/// module docs.
///
/// Must **not** be leaked (e.g. via `mem::forget`) while it borrows
/// non-`'static` data — that is the safety obligation of
/// [`Threadpool::spawn_local`], the `unsafe` fn that produces it.
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct SpawnFuture<'pool, R> {
	runnable: Option<Runnable>,
	task: Option<Inner<R>>,
	/// Phantom borrow of the pool — ties the future's lifetime to the
	/// [`Threadpool`] reference it was created from, ensuring the pool
	/// outlives any in-flight tasks.
	_pool: PhantomData<&'pool Threadpool>,
}

impl<'pool, R> SpawnFuture<'pool, R> {
	#[inline]
	pub(crate) fn new(runnable: Runnable, task: Inner<R>) -> Self {
		Self {
			runnable: Some(runnable),
			task: Some(task),
			_pool: PhantomData,
		}
	}
}

impl<R> Future for SpawnFuture<'_, R> {
	type Output = R;

	fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		let this = unsafe { self.as_mut().get_unchecked_mut() };
		if let Some(runnable) = this.runnable.take() {
			runnable.schedule();
		}
		if let Some(task) = this.task.as_mut() {
			match Pin::new(task).poll(cx) {
				Poll::Ready(result) => {
					this.task = None;
					match result {
						Ok(value) => Poll::Ready(value),
						Err(payload) => panic::resume_unwind(payload),
					}
				}
				Poll::Pending => Poll::Pending,
			}
		} else {
			panic!("SpawnFuture polled after completion");
		}
	}
}

impl<R> Drop for SpawnFuture<'_, R> {
	fn drop(&mut self) {
		if let Some(runnable) = self.runnable.take() {
			drop(runnable);
			drop(self.task.take());
		} else if let Some(task) = self.task.take() {
			block_on_cancel(task);
		}
	}
}

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
