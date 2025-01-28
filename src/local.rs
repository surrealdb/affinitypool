use std::{
	any::Any,
	future::Future,
	mem,
	panic::{self, AssertUnwindSafe},
	pin::Pin,
	sync::{Arc, Condvar, Mutex},
	task::{ready, Context, Poll},
};

use crate::{atomic_waker::AtomicWaker, task::OwnedTask, Threadpool};

struct SpawnFutureData<T> {
	// cond var to wait on the result of the mutex changing when we find it empty during block.
	condvar: Condvar,
	// The actual value, if the future is properly driven to completion we never block on the mutex.
	result: Mutex<Option<Result<T, Box<dyn Any + Send>>>>,
	// Waker to notify the runtime of completion of the task.
	waker: AtomicWaker,
}

enum State<'pool, F, R> {
	Init(F),
	Sending(async_channel::Send<'pool, OwnedTask<'static>>, Arc<SpawnFutureData<R>>),
	Running(Arc<SpawnFutureData<R>>),
	Done,
}

unsafe impl<T> Send for SpawnFutureData<T> {}
unsafe impl<T> Sync for SpawnFutureData<T> {}

#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct SpawnFuture<'pool, F, R> {
	pool: &'pool Threadpool,
	state: State<'pool, F, R>,
}

impl<'pool, F, R> SpawnFuture<'pool, F, R>
where
	F: FnOnce() -> R + Send,
	R: Send,
{
	pub(crate) fn new(pool: &'pool Threadpool, f: F) -> Self {
		SpawnFuture {
			pool,
			state: State::Init(f),
		}
	}
}

impl<F, T> Future for SpawnFuture<'_, F, T>
where
	F: FnOnce() -> T + Send,
	T: Send,
{
	type Output = T;

	fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		// pin is structural for everything.
		// pinning maintained by function impl.

		loop {
			match unsafe { &mut self.as_mut().get_unchecked_mut().state } {
				State::Init(_) => {
					let State::Init(task) = std::mem::replace(
						unsafe { &mut self.as_mut().get_unchecked_mut().state },
						State::Done,
					) else {
						unreachable!()
					};

					let data = Arc::new(SpawnFutureData {
						condvar: Condvar::new(),
						result: Mutex::new(None),
						waker: AtomicWaker::new(),
					});

					// We need to register a waker immediatly so that if the task finishes before
					// this thread can transition to State::Running ther is waker present to wake
					// the future.
					data.waker.register(cx.waker());

					let data_clone = data.clone();
					// send the task of to the thread no we are sure SpawnFuture will drop and not
					// move.
					let task = OwnedTask::new(move || {
						// keep the lock until we are done.
						{
							let mut lock = data_clone.result.lock().unwrap();
							let res = panic::catch_unwind(AssertUnwindSafe(task));

							*lock = Some(res);

							// drop the lock before waking the future.
							mem::drop(lock);
						}

						// wake the future so that it can retrieve the result.
						data_clone.waker.wake();
						// notify possible blocked threads of completion.
						data_clone.condvar.notify_one();
					});
					let future = unsafe { self.pool.data.sender.send(task.erase_lifetime()) };
					unsafe {
						self.as_mut().get_unchecked_mut().state = State::Sending(future, data);
					}
				}
				State::Sending(ref mut future, ref data) => {
					// Make sure the right waker is in place such that we get notified the moment a
					// the task is ready.
					data.waker.register(cx.waker());
					// pinning is structural for State::Sending and maintained by the
					// implementation
					unsafe { ready!(Pin::new_unchecked(future).poll(cx)) }.unwrap();

					let State::Sending(_, data) = std::mem::replace(
						unsafe { &mut self.as_mut().get_unchecked_mut().state },
						State::Done,
					) else {
						unreachable!()
					};

					unsafe {
						self.as_mut().get_unchecked_mut().state = State::Running(data);
					}
				}
				State::Running(data) => {
					let res = {
						let Some(mut guard) = data.result.try_lock().ok() else {
							data.waker.register(cx.waker());
							return Poll::Pending;
						};

						let Some(res) = guard.take() else {
							data.waker.register(cx.waker());
							return Poll::Pending;
						};
						res
					};

					unsafe {
						self.as_mut().get_unchecked_mut().state = State::Done;
					}

					return Poll::Ready(res.unwrap_or_else(|p| panic::resume_unwind(p)));
				}
				State::Done => {
					// Called after the future was already done, there is nothing reasonably we can do
					// here except panic.
					unsafe { self.get_unchecked_mut().state = State::Done };
					panic!("Tried to poll a SpawnFuture which was already done")
				}
			};
		}
	}
}

impl<F, T> Drop for SpawnFuture<'_, F, T> {
	fn drop(&mut self) {
		match self.state {
			State::Init(_) => {}
			State::Sending(_, _) => {}
			State::Running(ref data) => {
				let guard = data.result.lock().unwrap();

				// result was not yet ready, wait until it is finshed.
				if guard.is_none() {
					mem::drop(data.condvar.wait(guard).unwrap());
				}
			}
			State::Done => {}
		}
	}
}
