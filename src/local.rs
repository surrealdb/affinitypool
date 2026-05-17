use std::{
	any::Any,
	future::Future,
	panic::{self, AssertUnwindSafe},
	pin::Pin,
	sync::{Arc, Condvar, Mutex},
	task::{Context, Poll},
};

use crate::{Threadpool, atomic_waker::AtomicWaker, task::OwnedTask};

struct SpawnFutureData<T> {
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
///
/// # Leak safety
///
/// `SpawnFuture` carries a borrow with lifetime `'pool` and erases that
/// lifetime internally via [`OwnedTask::erase_lifetime`] before pushing the
/// task to the global injector. Soundness of `spawn_local` therefore does
/// **not** depend on this future's `Drop` running.
///
/// If the future is `mem::forget`-ed instead of dropped, the
/// `Arc<SpawnFutureData>` (and through it, the captured closure plus any
/// `'pool` borrows it holds) stays alive. The only references to that data
/// live inside the worker-owned `OwnedTask` and the leaked future itself.
/// The worker runs the closure at most once and then drops its `Arc`, after
/// which any remaining strong count is held by the leaked future — i.e.
/// leaked, but never accessed after its lifetime has ended. This is the
/// standard Rust leak-amplification rule: `mem::forget` is safe but may
/// leak memory.
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct SpawnFuture<'pool, F, R> {
	pool: &'pool Threadpool,
	state: State<F, R>,
}

// Safety: `SpawnFutureData<T>` is only ever shared with another thread via
// the worker closure that executes the user task. That closure produces a
// `T`, stores it, and the polling thread later reads it back — so `T` must
// be `Send`. The `AtomicWaker` and `Mutex<Option<…>>`/`Condvar` provide the
// synchronisation; declaring the impls conditional on `T: Send` is the
// minimum sound bound.
unsafe impl<T: Send> Send for SpawnFutureData<T> {}
unsafe impl<T: Send> Sync for SpawnFutureData<T> {}

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
			match this.state {
				State::Init(_) => {
					// Transition to a Done state before we submit the task to avoid race conditions.
					let State::Init(task) = std::mem::replace(&mut this.state, State::Done) else {
						unreachable!()
					};
					// Create the data for the future.
					let data = Arc::new(SpawnFutureData {
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
				State::Running(ref data) => {
					// Clone the Arc so we can work with it without borrowing from state
					let data = data.clone();
					// Try to get the result
					match data.result.try_lock() {
						Ok(mut guard) => {
							if let Some(res) = guard.take() {
								// Result is ready, drop the guard first
								drop(guard);
								// Now we can modify state
								this.state = State::Done;
								return Poll::Ready(
									res.unwrap_or_else(|p| panic::resume_unwind(p)),
								);
							} else {
								// Task not complete yet
								drop(guard);
								data.waker.register(cx.waker());
								return Poll::Pending;
							}
						}
						Err(_) => {
							// Task is currently writing result
							data.waker.register(cx.waker());
							return Poll::Pending;
						}
					}
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
			// Block and wait for completion
			let guard = data.result.lock().unwrap();
			// Wait until result is ready (handling spurious wakeups)
			let _guard = data.condvar.wait_while(guard, |x| x.is_none()).unwrap();
		}
	}
}
