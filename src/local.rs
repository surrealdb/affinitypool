use std::{
	any::Any,
	future::Future,
	panic::{self, AssertUnwindSafe},
	pin::Pin,
	sync::{
		Arc, Condvar, Mutex,
		atomic::{AtomicBool, Ordering},
	},
	task::{Context, Poll},
};

use crate::{Threadpool, atomic_waker::AtomicWaker, task::OwnedTask};

struct SpawnFutureData<T> {
	// Atomic flag to check if result is ready without taking a lock
	ready: AtomicBool,
	// cond var to wait on the result of the mutex changing when we find it empty during block.
	condvar: Condvar,
	// Waker to notify the runtime of completion of the task.
	waker: AtomicWaker,
	// The actual value, if the future is properly driven to completion we never block on the mutex.
	result: Mutex<Option<Result<T, Box<dyn Any + Send>>>>,
}

enum State<F, R> {
	Init(F),
	Running(Arc<SpawnFutureData<R>>),
	Done,
}

/// A future that spawns a task on the threadpool and returns the result.
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct SpawnFuture<'pool, F, R> {
	pool: &'pool Threadpool,
	state: State<F, R>,
}

unsafe impl<T> Send for SpawnFutureData<T> {}
unsafe impl<T> Sync for SpawnFutureData<T> {}

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
		// Pin is structural for everything, and is maintained by the function impl.
		loop {
			// Safety: We're maintaining pinning guarantees throughout
			let this = unsafe { self.as_mut().get_unchecked_mut() };
			// Match on the current state of the future.
			match &mut this.state {
				State::Init(_) => {
					// Transition to a Done state before we submit the task to avoid race conditions.
					let State::Init(task) = std::mem::replace(&mut this.state, State::Done) else {
						unreachable!()
					};
					// Create the data for the future.
					let data = Arc::new(SpawnFutureData {
						ready: AtomicBool::new(false),
						condvar: Condvar::new(),
						result: Mutex::new(None),
						waker: AtomicWaker::new(),
					});
					// Register waker before submitting task to avoid race condition.
					data.waker.register(cx.waker());
					// Clone the data for the future.
					let data_clone = data.clone();
					// Create the task to run the passed function.
					let task = OwnedTask::new(move || {
						// Execute task and store result.
						{
							// Store the result of the task in the mutex.
							let mut lock = data_clone.result.lock().unwrap();
							// Run the task itself, and catch any panics.
							let res = panic::catch_unwind(AssertUnwindSafe(task));
							// Store the result in the mutex.
							*lock = Some(res);
							// Lock drops here automatically.
						}
						// Mark result as ready (memory fence before waking)
						data_clone.ready.store(true, Ordering::Release);
						// Wake the future.
						data_clone.waker.wake();
						// Notify any blocked threads.
						data_clone.condvar.notify_one();
					});
					// Safety: task lifetime is erased but we maintain it via Arc<SpawnFutureData>
					unsafe {
						// Push the task to the global injector.
						this.pool.data.injector.push(task.erase_lifetime());
					}
					// Wake up a parked worker thread.
					if let Some(thread) = this.pool.data.parked_threads.pop() {
						thread.unpark();
					}
					// Transition this future to the Running state.
					this.state = State::Running(data);
				}
				State::Running(data) => {
					// Check if result is ready without locking.
					if !data.ready.load(Ordering::Acquire) {
						data.waker.register(cx.waker());
						return Poll::Pending;
					}
					// Result is ready, take it.
					let res = {
						let mut guard = data.result.lock().unwrap();
						guard.take().expect("ready flag was true but result was None")
					};
					// Mark this future as done.
					this.state = State::Done;
					// Mark this future as ready.
					return Poll::Ready(res.unwrap_or_else(|p| panic::resume_unwind(p)));
				}
				State::Done => {
					// We should never get to this state by polling a future once done.
					panic!("SpawnFuture polled after completion")
				}
			};
		}
	}
}

impl<F, T> Drop for SpawnFuture<'_, F, T> {
	fn drop(&mut self) {
		if let State::Running(ref data) = self.state {
			// Fast path: check if already complete
			if data.ready.load(Ordering::Acquire) {
				return;
			}
			// Slow path: block and wait for completion
			let guard = data.result.lock().unwrap();
			// Wait until result is ready (handling spurious wakeups)
			let _guard = data.condvar.wait_while(guard, |x| x.is_none()).unwrap();
		}
	}
}
