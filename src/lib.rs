pub mod affinity;
mod atomic_waker;
mod builder;
mod data;
mod error;
mod global;
mod local;
mod sentry;
mod task;

pub use crate::builder::Builder;
pub use crate::error::Error;

use crate::data::Data;
use crate::global::THREADPOOL;
use crate::sentry::Sentry;
use crossbeam::deque::{Injector, Worker};
use crossbeam::queue::ArrayQueue;
use local::SpawnFuture;
use parking_lot::RwLock;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use task::OwnedTask;
use tokio::sync::oneshot;

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
	data: Arc<Data>,
}

impl Default for Threadpool {
	fn default() -> Self {
		Threadpool::new(num_cpus::get())
	}
}

impl Threadpool {
	/// Create a new thread pool.
	pub fn new(workers: usize) -> Self {
		// Validate worker count
		let workers = workers.clamp(1, MAX_THREADS);
		// Create a global injector for tasks
		let injector = Injector::new();
		// Create workers and collect their stealers
		let mut stealers = Vec::with_capacity(workers);
		let mut worker_queues = Vec::with_capacity(workers);
		// Create a Worker deque for each thread
		for _ in 0..workers {
			let worker = Worker::new_fifo();
			stealers.push(worker.stealer());
			worker_queues.push(worker);
		}
		// Create the threadpool shared data
		let data = Arc::new(Data {
			name: None,
			stack_size: None,
			num_threads: AtomicUsize::new(workers),
			thread_count: AtomicUsize::new(0),
			injector,
			stealers: RwLock::new(stealers),
			parked_threads: ArrayQueue::new(workers),
			shutdown: AtomicBool::new(false),
			thread_handles: RwLock::new(Vec::new()),
		});
		// Spawn the desired number of workers
		for (index, worker) in worker_queues.into_iter().enumerate() {
			Self::spin_up(None, data.clone(), worker, index);
		}
		// Return the new threadpool
		Threadpool {
			data,
		}
	}

	/// Queue a new command for execution on this pool.
	pub async fn spawn<F, R>(&self, func: F) -> R
	where
		F: FnOnce() -> R,
		F: Send + 'static,
		R: Send + 'static,
	{
		// Create a new oneshot channel
		let (tx, rx) = oneshot::channel();
		// Enclose the function in a closure
		let task = OwnedTask::new(move || {
			tx.send(catch_unwind(AssertUnwindSafe(func))).ok();
		});
		// Push the task to the global injector
		self.data.injector.push(task);
		// Wake up a parked worker thread
		if let Some(thread) = self.data.parked_threads.pop() {
			thread.unpark();
		}
		// The channel has not been closed
		let res = rx.await.unwrap();
		// Wait for the function response
		res.unwrap_or_else(|err| resume_unwind(err))
	}

	/// Queue a new command for execution on this pool with access to the local variables.
	///
	/// The future of this function will block the current thread if it is not fully completed.
	pub fn spawn_local<'pool, F, R>(&'pool self, func: F) -> SpawnFuture<'pool, F, R>
	where
		F: FnOnce() -> R,
		F: Send + 'pool,
		R: Send + 'pool,
	{
		SpawnFuture::new(self, func)
	}

	/// Set this threadpool as the global threadpool.
	pub fn build_global(self) -> Result<(), Error> {
		// Check if the threadpool has been created
		if THREADPOOL.get().is_some() {
			return Err(Error::GlobalThreadpoolExists);
		}
		// Set this threadpool as the global threadpool
		THREADPOOL.get_or_init(|| self);
		// Global threadpool was created successfully
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

	/// Finds the next task to execute.
	///
	/// Following the work-stealing pattern from crossbeam:
	/// 1. Try to pop from the local worker queue
	/// 2. Try to steal from the global injector
	/// 3. Try to steal from other workers (if multi-threaded)
	fn find_task(
		local: &Worker<OwnedTask<'static>>,
		data: &Arc<Data>,
		index: usize,
	) -> Option<OwnedTask<'static>> {
		// Pop a task from the local queue, if not empty.
		local.pop().or_else(|| {
			// Track the number of retry loops
			let mut retries = 0;
			// Otherwise, we need to look for a task elsewhere.
			std::iter::repeat_with(|| {
				// Add spin hint on retries to reduce contention
				if retries > 0 {
					std::hint::spin_loop();
				}
				// Increment the number of retries
				retries += 1;
				// Try stealing a batch of tasks from the global queue.
				let result = data.injector.steal_batch_and_pop(local);
				// If there's work in the queue, wake a thread to help
				if !data.injector.is_empty()
					&& let Some(thread) = data.parked_threads.pop()
				{
					thread.unpark();
				}
				// Return the stolen task, if there is one
				result
					// Or try stealing from one of the other threads
					.or_else(|| {
						// Try to steal from other workers, excluding our own
						// Acquire read lock only when needed for stealing
						let stealers = data.stealers.read();
						stealers
							.iter()
							.enumerate()
							.filter(|(i, _)| *i != index) // Don't steal from ourselves
							.map(|(_, s)| s.steal())
							.find(|s| !s.is_retry())
							.unwrap_or(crossbeam::deque::Steal::Empty)
					})
			})
			// Loop while no task was stolen and any steal operation needs to be retried.
			.find(|s| !s.is_retry())
			// Extract the stolen task, if there is one.
			.and_then(|s| s.success())
		})
	}

	/// Spins up a new worker thread in this pool.
	#[cfg(not(target_family = "wasm"))]
	fn spin_up(
		coreid: Option<usize>,
		data: Arc<Data>,
		local: Worker<OwnedTask<'static>>,
		index: usize,
	) {
		// Create a new thread builder
		let mut builder = std::thread::Builder::new();
		// Assign a name to the threads if specified
		if let Some(ref name) = data.name {
			builder = builder.name(name.clone());
		}
		// Assign a stack size to the threads if specified
		if let Some(stack_size) = data.stack_size {
			builder = builder.stack_size(stack_size);
		}
		// Increase the thread count counter
		data.thread_count.fetch_add(1, Ordering::Relaxed);
		// Create a new sentry watcher
		let sentry = Sentry::new(coreid, index, Arc::downgrade(&data));
		// Clone data for handle storage
		let data_clone = data.clone();
		// Spawn a new worker thread
		let handle = builder.spawn(move || {
			// Assign this thread to a core
			if let Some(coreid) = coreid {
				affinity::set_for_current(coreid.into());
			}
			// Loop continuously, processing any jobs
			loop {
				// Check if we should shut down
				if data.shutdown.load(Ordering::Acquire) {
					break;
				}
				// Try to find a task using work-stealing
				if let Some(task) = Self::find_task(&local, &data, index) {
					// Process the task
					task.run();
				} else {
					// No work found, so register ourselves as parked
					let _ = data.parked_threads.push(std::thread::current());
					// Double-check for work after registering
					if !data.injector.is_empty() {
						// Work just arrived, try to wake a thread
						if let Some(t) = data.parked_threads.pop() {
							// Always unpark the thread we popped
							t.unpark();
						}
					}
					// Park this thread for a short time
					std::thread::park_timeout(Duration::from_millis(10));
				}
			}
			// This thread has exited cleanly
			sentry.cancel();
		});

		// Store the thread handle if spawning succeeded
		if let Ok(handle) = handle {
			data_clone.thread_handles.write().push(handle);
		}
	}

	/// Spins up a new worker thread in this pool.
	#[cfg(target_family = "wasm")]
	fn spin_up(
		coreid: Option<usize>,
		data: Arc<Data>,
		local: Worker<OwnedTask<'static>>,
		index: usize,
	) {
		// Do nothing in WASM
	}
}

impl Drop for Threadpool {
	fn drop(&mut self) {
		// Signal all workers to shut down
		self.data.shutdown.store(true, Ordering::Release);
		// Wake up all parked workers
		while let Some(thread) = self.data.parked_threads.pop() {
			thread.unpark();
		}
		// Take all of the thread handles for joining
		let handles = self.data.thread_handles.write().drain(..).collect::<Vec<_>>();
		// Join all worker threads
		for handle in handles {
			// Wait for the thread to exit
			let _ = handle.join();
		}
		// Decrement thread count to 0
		self.data.thread_count.store(0, Ordering::Relaxed);
	}
}
