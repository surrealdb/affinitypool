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

use async_task::{Runnable, Task};
use std::any::Any;
use std::future::Future;
use std::marker::PhantomData;
use std::mem;
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

/// State machine for the spawn-local future. We defer
/// [`Runnable::schedule`] to first poll so a 1-worker pool cannot
/// deadlock when a caller does `drop(pool.spawn_local(…))` from inside
/// the only worker thread.
enum State<R> {
	/// Constructed but not yet polled. Holds the unscheduled
	/// [`Runnable`] alongside the [`Task`] so first poll can push the
	/// runnable into the queue, and so an early drop can release the
	/// runnable without ever touching a worker.
	Pending {
		runnable: Runnable,
		task: Inner<R>,
	},
	/// First poll has already pushed the runnable onto the queue. Only
	/// the awaiter handle remains.
	Scheduled(Inner<R>),
	/// Resolved or dropped — no further work.
	Done,
}

/// A future returned by [`Threadpool::spawn_local`].
///
/// Resolves to the closure's return value. The runnable is scheduled
/// lazily on first poll — constructing a `SpawnFuture` and dropping it
/// without polling never touches a worker. Dropping after polling
/// cancels the task; if the worker is currently running the closure,
/// the dropping thread is parked until the worker has finished. See
/// module docs.
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct SpawnFuture<'pool, R> {
	state: State<R>,
	/// Phantom borrow of the pool — ties the future's lifetime to the
	/// [`Threadpool`] reference it was created from, ensuring the pool
	/// outlives any in-flight tasks.
	_pool: PhantomData<&'pool Threadpool>,
}

impl<'pool, R> SpawnFuture<'pool, R> {
	#[inline]
	pub(crate) fn new(runnable: Runnable, task: Inner<R>) -> Self {
		Self {
			state: State::Pending {
				runnable,
				task,
			},
			_pool: PhantomData,
		}
	}
}

impl<R> Future for SpawnFuture<'_, R> {
	type Output = R;

	fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		dbg!("called 3");
		// Structural pinning: we never move out of `self.state` except
		// to replace it whole, and the Task inside is moved only when
		// it has just been constructed by `mem::replace` (i.e. is no
		// longer pinned through this Self).
		let this = unsafe { self.as_mut().get_unchecked_mut() };
		// Schedule on first poll. Transitions `Pending → Scheduled`
		// and then falls through to poll the Task immediately so a
		// caller awaiting the future doesn't need a second poll just
		// to register the waker.
		if matches!(this.state, State::Pending { .. }) {
			dbg!("called 4");
			match mem::replace(&mut this.state, State::Done) {
				State::Pending {
					runnable,
					task,
				} => {
					runnable.schedule();
					this.state = State::Scheduled(task);
				}
				_ => unreachable!(),
			}
		}
		match &mut this.state {
			State::Scheduled(task) => match Pin::new(task).poll(cx) {
				Poll::Ready(result) => {
					this.state = State::Done;
					match result {
						Ok(value) => Poll::Ready(value),
						Err(payload) => panic::resume_unwind(payload),
					}
				}
				Poll::Pending => Poll::Pending,
			},
			State::Done => panic!("SpawnFuture polled after completion"),
			State::Pending {
				..
			} => unreachable!("Pending handled above"),
		}
	}
}

impl<R> Drop for SpawnFuture<'_, R> {
	fn drop(&mut self) {
		match mem::replace(&mut self.state, State::Done) {
			State::Pending {
				runnable,
				task,
			} => {
				// Never scheduled — dropping the runnable cancels the
				// task synchronously without touching a worker, so no
				// `block_on_cancel` is needed and a 1-worker pool
				// cannot deadlock here.
				drop(runnable);
				drop(task);
			}
			State::Scheduled(task) => {
				// `Task::cancel()` returns a future that resolves once
				// the runnable has finished (either by running to
				// completion or by being dropped without running).
				// Block the current thread on that — the closure may
				// borrow `'pool` data that goes out of scope as soon
				// as this returns.
				block_on_cancel(task);
			}
			State::Done => {}
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
