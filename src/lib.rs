pub mod affinity;
mod builder;
mod cpu;
mod data;
mod error;
mod global;
mod local;
mod queue;
mod sentry;

pub use crate::builder::Builder;
pub use crate::error::Error;
pub use crate::local::SpawnFuture;

use crate::data::Data;
use crate::global::THREADPOOL;
use crate::queue::Queue;
use crate::sentry::Sentry;
use async_task::{Builder as TaskBuilder, Runnable};
use parking_lot::Mutex;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Maximum number of worker threads allowed in a thread pool.
pub const MAX_THREADS: usize = 512;

/// Queue a new command for execution on the global threadpool.
///
/// If no global threadpool has been created, then this function
/// runs the provided closure immediately, without passing it to
/// a blocking threadpool.
pub async fn spawn<F, R>(func: F) -> R
where
	F: FnOnce() -> R + Send + 'static,
	R: Send + 'static,
{
	if let Some(threadpool) = THREADPOOL.get() {
		threadpool.spawn(func).await
	} else {
		func()
	}
}

/// Queue a new command for execution on the global threadpool.
/// The future of this function will block the current thread
/// if it is not fully awaited and driven to completion.
///
/// If no global threadpool has been created, then this function
/// runs the provided closure immediately, without passing it to
/// a blocking threadpool.
pub async fn spawn_local<'pool, F, R>(func: F) -> R
where
	F: FnOnce() -> R,
	F: Send + 'pool,
	R: Send + 'pool,
{
	if let Some(threadpool) = THREADPOOL.get() {
		threadpool.spawn_local(func).await
	} else {
		func()
	}
}

pub struct Threadpool {
	pub(crate) data: Arc<Data>,
}

impl Default for Threadpool {
	fn default() -> Self {
		Threadpool::new(num_cpus::get())
	}
}

impl Threadpool {
	/// Create a new thread pool.
	pub fn new(workers: usize) -> Self {
		// Validate worker count.
		let workers = workers.clamp(1, MAX_THREADS);
		// Create the threadpool shared data.
		let data = Arc::new(Data {
			name: None,
			stack_size: None,
			num_threads: AtomicUsize::new(workers),
			thread_count: AtomicUsize::new(0),
			queue: Arc::new(Queue::new(workers)),
			thread_handles: Mutex::new(Vec::new()),
		});
		// Spawn the desired number of workers.
		for index in 0..workers {
			Self::spin_up(None, data.clone(), index);
		}
		// Return the new threadpool.
		Threadpool {
			data,
		}
	}

	/// Queue a new command for execution on this pool.
	///
	/// The closure is scheduled **immediately** — by the time this
	/// function returns, the task is already on the worker queue and
	/// may already be running. The returned future resolves to the
	/// closure's return value. Dropping the future before completion
	/// cancels the task: a queued-but-unrun task is dropped without
	/// running; a currently-running task completes but its result is
	/// discarded.
	///
	/// Returning a future (rather than being an `async fn`) means
	/// pipelined producers — e.g. `let h1 = pool.spawn(a); let h2 =
	/// pool.spawn(b)` — start both `a` and `b` on workers
	/// concurrently, before any `.await`. This matches the semantics
	/// of `tokio::task::spawn_blocking`.
	pub fn spawn<F, R>(&self, func: F) -> impl Future<Output = R> + Send + 'static
	where
		F: FnOnce() -> R,
		F: Send + 'static,
		R: Send + 'static,
	{
		// Clone the Arc<Queue> into the schedule closure. The closure
		// is `Fn(Runnable) + Send + Sync + 'static`, which is what
		// async-task requires.
		let queue = self.data.queue.clone();
		let schedule = move |runnable: Runnable| queue.push(runnable);
		// Wrap the closure in `catch_unwind` so a panic inside the
		// closure is reified into a `Result::Err(payload)` on the
		// worker side instead of propagating up through
		// `Runnable::run` and unwinding the worker thread. The
		// awaiter then `resume_unwind`s the payload, matching the
		// previous behaviour where panics surfaced on the `.await`
		// side, not the worker side. Without this wrapper, a
		// panicking closure would tear down the worker and force the
		// sentry to respawn a thread on every panic.
		let (runnable, task) = TaskBuilder::new().spawn(
			move |()| async move { std::panic::catch_unwind(std::panic::AssertUnwindSafe(func)) },
			schedule,
		);
		// Push the runnable onto the queue right now so a worker can
		// start processing it before the caller awaits.
		runnable.schedule();
		// Returning the future to the caller — dropping it cancels.
		// The outer async block re-raises any captured panic, so the
		// future's output type stays `R` (not `Result<R, _>`).
		async move {
			match task.await {
				Ok(value) => value,
				Err(payload) => std::panic::resume_unwind(payload),
			}
		}
	}

	/// Queue a new command for execution on this pool with access to
	/// the local variables.
	///
	/// The returned future blocks the current thread on `Drop` if the
	/// closure is currently being executed (so closure borrows of
	/// `'pool` data cannot dangle).
	pub fn spawn_local<'pool, F, R>(&'pool self, func: F) -> SpawnFuture<'pool, R>
	where
		F: FnOnce() -> R,
		F: Send + 'pool,
		R: Send + 'pool,
	{
		let queue = self.data.queue.clone();
		let schedule = move |runnable: Runnable| queue.push(runnable);
		// Same `catch_unwind` wrapper rationale as `spawn` — keep
		// panics off the worker thread, surface them on the awaiter.
		// SAFETY: `async_task::Builder::spawn_unchecked` lifts the
		// `'static` bound on the future. Soundness is preserved by:
		//   1. `SpawnFuture<'pool, R>` carries the `'pool` borrow, so
		//      the future cannot outlive the pool.
		//   2. `SpawnFuture::drop` blocks (via `Task::cancel().await`)
		//      until the runnable has stopped, so the closure's
		//      `'pool` borrows are not accessed after they expire.
		//   3. The same `mem::forget` caveat as the previous
		//      implementation applies — see `local.rs` module docs.
		let (runnable, task) = unsafe {
			TaskBuilder::new().spawn_unchecked(
				move |()| async move { std::panic::catch_unwind(std::panic::AssertUnwindSafe(func)) },
				schedule,
			)
		};
		runnable.schedule();
		SpawnFuture::new(task)
	}

	/// Set this threadpool as the global threadpool.
	pub fn build_global(self) -> Result<(), Error> {
		// Check if the threadpool has been created.
		if THREADPOOL.get().is_some() {
			return Err(Error::GlobalThreadpoolExists);
		}
		// Set this threadpool as the global threadpool.
		THREADPOOL.get_or_init(|| self);
		// Global threadpool was created successfully.
		Ok(())
	}

	/// Get the total number of worker threads in this pool.
	pub fn thread_count(&self) -> usize {
		self.data.thread_count.load(Ordering::Relaxed)
	}

	/// Get the specified number of threads for this pool.
	pub fn num_threads(&self) -> usize {
		self.data.num_threads.load(Ordering::Relaxed)
	}

	/// Spin up a new worker thread in this pool.
	#[cfg(not(target_family = "wasm"))]
	pub(crate) fn spin_up(coreid: Option<usize>, data: Arc<Data>, index: usize) {
		// Create a new thread builder.
		let mut builder = std::thread::Builder::new();
		// Assign a name to the thread if specified.
		if let Some(ref name) = data.name {
			builder = builder.name(name.clone());
		}
		// Assign a stack size to the thread if specified.
		if let Some(stack_size) = data.stack_size {
			builder = builder.stack_size(stack_size);
		}
		// Increase the thread count counter.
		data.thread_count.fetch_add(1, Ordering::Relaxed);
		// Create a new sentry watcher.
		let sentry = Sentry::new(coreid, index, Arc::downgrade(&data));
		// Clone the queue handle for the worker loop.
		let queue = data.queue.clone();
		// Clone data for handle storage.
		let data_clone = data.clone();
		// Spawn a new worker thread.
		let handle = builder.spawn(move || {
			// Assign this thread to a core.
			if let Some(coreid) = coreid {
				affinity::set_for_current(coreid.into());
			}
			// Worker loop: pop a runnable, run it, repeat. The worker
			// passes its `index` so the queue can route the pop to
			// its preferred shard, falling back to scanning peers.
			// Parking is handled by `Queue::pop_blocking` via its
			// Condvar.
			while let Some(runnable) = queue.pop_blocking(index) {
				runnable.run();
			}
			// Clean exit — cancel the sentry so its Drop does not
			// trigger a respawn.
			sentry.cancel();
		});
		// Store the thread handle if spawning succeeded.
		if let Ok(handle) = handle {
			data_clone.thread_handles.lock().push(handle);
		}
	}

	/// WASM stub — affinity and worker threads are not supported.
	#[cfg(target_family = "wasm")]
	pub(crate) fn spin_up(_coreid: Option<usize>, _data: Arc<Data>, _index: usize) {
		// Do nothing in WASM.
	}
}

impl Drop for Threadpool {
	fn drop(&mut self) {
		// Signal all workers to shut down. The Condvar wakes everyone
		// blocked in `pop_blocking`; once the queue drains, each worker
		// observes `None` and exits.
		self.data.queue.shutdown();
		// Take all of the thread handles for joining.
		let handles = self.data.thread_handles.lock().drain(..).collect::<Vec<_>>();
		// Join all worker threads.
		for handle in handles {
			let _ = handle.join();
		}
		// Decrement thread count to 0.
		self.data.thread_count.store(0, Ordering::Relaxed);
	}
}
