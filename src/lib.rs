mod builder;
mod data;
mod error;
mod global;
mod sentry;
mod task;

pub use crate::builder::Builder;
pub use crate::error::Error;

use crate::data::Data;
use crate::global::THREADPOOL;
use crate::sentry::Sentry;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::oneshot;

/// Queue a new command for execution on the global threadpool
///
/// # Panics
///
/// This function panics if a global threadpool has not been created.
pub async fn execute<F, R>(func: F) -> R
where
	F: FnOnce() -> R + Send + 'static,
	R: Send + 'static,
{
	THREADPOOL.get().unwrap().execute(func).await
}

#[derive(Debug)]
pub struct Threadpool {
	data: Arc<Data>,
}

impl Default for Threadpool {
	fn default() -> Self {
		Threadpool::new(num_cpus::get())
	}
}

impl Threadpool {
	/// Create a new thread pool
	pub fn new(workers: usize) -> Self {
		// Create a queuing channel for tasks
		let (send, recv) = async_channel::unbounded();
		// Create the threadpool shared data
		let data = Arc::new(Data {
			name: None,
			stack_size: None,
			max_threads: AtomicUsize::new(workers),
			thread_count: AtomicUsize::new(0),
			queued_count: AtomicUsize::new(0),
			active_count: AtomicUsize::new(0),
			sender: send,
			receiver: recv,
		});
		// Spawn the desired number of workers
		for _ in 0..workers {
			Self::spawn(None, data.clone());
		}
		// Return the new threadpool
		Threadpool {
			data,
		}
	}

	/// Queue a new command for execution on this pool
	pub async fn execute<F, R>(&self, func: F) -> R
	where
		F: FnOnce() -> R + Send + 'static,
		R: Send + 'static,
	{
		// Create a new oneshot channel
		let (tx, rx) = oneshot::channel();
		// Enclose the function in a closure
		let func = move || {
			tx.send(catch_unwind(AssertUnwindSafe(func))).ok();
		};
		// Increase the queued job counter
		self.data.queued_count.fetch_add(1, Ordering::SeqCst);
		// Send the function for processing
		self.data.sender.send(Box::new(func)).await.unwrap();
		// The channel has not been closed
		let res = rx.await.unwrap();
		// Wait for the function response
		res.unwrap_or_else(|err| resume_unwind(err))
	}

	/// Set this threadpool as the global threadpool
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

	/// Get the total number of worker threads in this pool
	pub fn thread_count(&self) -> usize {
		self.data.thread_count.load(Ordering::Relaxed)
	}

	/// Get the current number of queued jobs in this pool
	pub fn queued_count(&self) -> usize {
		self.data.queued_count.load(Ordering::Relaxed)
	}

	/// Get the current number of active jobs in this pool
	pub fn active_count(&self) -> usize {
		self.data.queued_count.load(Ordering::Relaxed)
	}

	/// Get the specified number of threads for this pool
	pub fn max_threads(&self) -> usize {
		self.data.max_threads.load(Ordering::Relaxed)
	}

	/// Set the desired number of workers in this pool
	pub fn set_workers(&mut self, workers: usize) {
		// Workers needs to be at least 1
		assert!(workers >= 1);
		// Get the current number of threads
		let current = self.data.max_threads.swap(workers, Ordering::Release);
		// Check if we should spawn new threads
		if let Some(additional) = workers.checked_sub(current) {
			// Spawn the new workers
			for _ in 0..additional {
				Self::spawn(None, self.data.clone());
			}
		}
	}

	/// Spawns a new worker thread in this pool
	fn spawn(_coreid: Option<usize>, data: Arc<Data>) {
		// Create a new thread builder
		let mut builder = std::thread::Builder::new();
		// Assign a name to the thrads if specified
		if let Some(ref name) = data.name {
			builder = builder.name(name.clone());
		}
		// Assign a stack size to the thrads if specified
		if let Some(stack_size) = data.stack_size {
			builder = builder.stack_size(stack_size);
		}
		// Spawn a new worker thread
		let _ = builder.spawn(move || {
			// Create a new sentry watcher
			let sentry = Sentry::new(&data);
			// Increase the thread count counter
			data.thread_count.fetch_add(1, Ordering::SeqCst);
			// Loop continuously, processing any jobs
			loop {
				// Get the max specified thread count
				let max_threads = data.max_threads.load(Ordering::Relaxed);
				// Get the total current thread count
				let thread_count = data.thread_count.load(Ordering::Acquire);
				// Check if there are too many threads
				if thread_count > max_threads {
					break;
				}
				// Pull a message from the job channel
				let job = match data.receiver.recv_blocking() {
					// We received a job to process
					Ok(job) => job,
					// This threadpool was dropped
					Err(_) => break,
				};
				// Decrease the queued job counter
				data.queued_count.fetch_sub(1, Ordering::Relaxed);
				// Increase the active job counter
				data.active_count.fetch_add(1, Ordering::Relaxed);
				// Process the function callback
				job.run();
				// Decrease the active job counter
				data.active_count.fetch_sub(1, Ordering::Relaxed);
			}
			// This thread has exited cleanly
			sentry.cancel();
		});
	}
}
