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
///
/// This function is safe, as long as the caller ensures that the returned
/// future is awaited and fully driven to completion. The caller must ensure
/// that the lifetime ’scope is valid until the returned future is fully driven.
///
/// # Panics
///
/// This function panics if a global threadpool has not been created.
pub async fn execute<'scope, F, R>(func: F) -> R
where
	F: FnOnce() -> R + Send + 'scope,
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
			num_threads: AtomicUsize::new(workers),
			thread_count: AtomicUsize::new(0),
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
	///
	/// This function is safe, as long as the caller ensures that the returned
	/// future is awaited and fully driven to completion. The caller must ensure
	/// that the lifetime ’scope is valid until the returned future is fully driven.
	pub async fn execute<'scope, F, R>(&self, func: F) -> R
	where
		F: FnOnce() -> R + Send + 'scope,
		R: Send + 'static,
	{
		// Create a new oneshot channel
		let (tx, rx) = oneshot::channel();
		// Remove the static requirement
		let func = unsafe {
			let boxed: Box<dyn FnOnce() -> R + Send + 'scope> = Box::new(func);
			std::mem::transmute::<
				Box<dyn FnOnce() -> R + Send + 'scope>,
				Box<dyn FnOnce() -> R + Send + 'static>,
			>(boxed)
		};
		// Enclose the function in a closure
		let func = move || {
			tx.send(catch_unwind(AssertUnwindSafe(func))).ok();
		};
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

	/// Get the specified number of threads for this pool
	pub fn num_threads(&self) -> usize {
		self.data.num_threads.load(Ordering::Relaxed)
	}

	/// Spawns a new worker thread in this pool
	fn spawn(coreid: Option<usize>, data: Arc<Data>) {
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
			let sentry = Sentry::new(coreid, &data);
			// Increase the thread count counter
			data.thread_count.fetch_add(1, Ordering::SeqCst);
			// Loop continuously, processing any jobs
			loop {
				// Pull a message from the job channel
				let job = match data.receiver.recv_blocking() {
					// We received a job to process
					Ok(job) => job,
					// This threadpool was dropped
					Err(_) => break,
				};
				// Process the function callback
				job.run();
			}
			// This thread has exited cleanly
			sentry.cancel();
		});
	}
}
