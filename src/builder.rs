use crate::Data;
use crate::MAX_THREADS;
use crate::Threadpool;
use crossbeam::deque::{Injector, Worker};
use crossbeam::queue::ArrayQueue;
use parking_lot::RwLock;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};

#[derive(Default, Clone)]
pub struct Builder {
	num_threads: Option<usize>,
	thread_name: Option<String>,
	thread_stack_size: Option<usize>,
	thread_per_core: bool,
}

impl Builder {
	/// Initiate a new [`Builder`].
	///
	/// # Examples
	///
	/// ```
	/// let builder = affinitypool::Builder::new();
	/// ```
	pub fn new() -> Builder {
		Builder {
			num_threads: None,
			thread_name: None,
			thread_stack_size: None,
			thread_per_core: false,
		}
	}

	/// Set the maximum number of worker-threads that will be alive at any given moment by the built
	/// [`Threadpool`]. If not specified, defaults the number of threads to the number of CPUs.
	///
	/// # Panics
	///
	/// This method will panic if `num_threads` is 0.
	///
	/// # Examples
	///
	/// No more than eight threads will be alive simultaneously for this pool:
	///
	/// ```
	/// use std::thread;
	///
	/// let pool = affinitypool::Builder::new()
	///         .worker_threads(8)
	///         .build();
	///
	/// # tokio::runtime::Runtime::new().unwrap().block_on(async {
	///     for _ in 0..10 {
	///         pool.spawn(|| {
	///             println!("Hello from a worker thread!")
	///         }).await;
	///     }
	/// # });
	/// ```
	pub fn worker_threads(mut self, num_threads: usize) -> Builder {
		self.num_threads = Some(num_threads);
		self
	}

	/// Set the thread name for each of the threads spawned by the built [`Threadpool`]. If not
	/// specified, threads spawned by the thread pool will be unnamed.
	///
	/// # Examples
	///
	/// Each thread spawned by this pool will have the name "foo":
	///
	/// ```
	/// use std::thread;
	///
	/// let pool = affinitypool::Builder::new()
	///     .thread_name("foo")
	///     .build();
	///
	/// # tokio::runtime::Runtime::new().unwrap().block_on(async {
	///     for _ in 0..10 {
	///         pool.spawn(|| {
	///             assert_eq!(thread::current().name(), Some("foo"));
	///         }).await;
	///     }
	/// # });
	/// ```
	pub fn thread_name(mut self, name: impl Into<String>) -> Builder {
		self.thread_name = Some(name.into());
		self
	}

	/// Set the stack size (in bytes) for each of the threads spawned by the built [`Threadpool`].
	/// If not specified, threads spawned by the threadpool will have a stack size [as specified in
	/// the `std::thread` documentation][thread].
	///
	/// # Examples
	///
	/// Each thread spawned by this pool will have a 4 MB stack:
	///
	/// ```
	/// let pool = affinitypool::Builder::new()
	///     .thread_stack_size(4_000_000)
	///     .build();
	///
	/// # tokio::runtime::Runtime::new().unwrap().block_on(async {
	///     for _ in 0..10 {
	///         pool.spawn(|| {
	///             println!("This thread has a 4 MB stack size!");
	///         }).await;
	///     }
	/// # });
	/// ```
	pub fn thread_stack_size(mut self, size: usize) -> Builder {
		self.thread_stack_size = Some(size);
		self
	}

	/// Set whether a thread should be spawned per core.
	///
	/// # Examples
	///
	/// Each thread spawned will be linked to a separate core:
	///
	/// ```
	/// let pool = affinitypool::Builder::new()
	///     .thread_per_core(true)
	///     .build();
	///
	/// # tokio::runtime::Runtime::new().unwrap().block_on(async {
	///     for _ in 0..10 {
	///         pool.spawn(|| {
	///             println!("This is executed on individual cores!");
	///         }).await;
	///     }
	/// # });
	/// ```
	pub fn thread_per_core(mut self, enabled: bool) -> Builder {
		self.thread_per_core = enabled;
		self
	}

	/// Finalize the [`Builder`] and build the [`Threadpool`].
	///
	/// # Examples
	///
	/// ```
	/// let pool = affinitypool::Builder::new()
	///     .worker_threads(8)
	///     .thread_stack_size(4_000_000)
	///     .build();
	/// ```
	pub fn build(self) -> Threadpool {
		// Calculate how many threads to spawn
		let threads = if let Some(num_threads) = self.num_threads {
			num_threads.clamp(1, MAX_THREADS)
		} else if self.thread_per_core {
			num_cpus::get().clamp(1, MAX_THREADS)
		} else {
			2
		};
		// Create a global injector for tasks
		let injector = Injector::new();
		// Create workers and collect their stealers
		let mut workers = Vec::with_capacity(threads);
		let mut stealers = Vec::with_capacity(threads);
		// Create a Worker deque for each thread
		for _ in 0..threads {
			let worker = Worker::new_fifo();
			stealers.push(worker.stealer());
			workers.push(worker);
		}
		// Create the threadpool shared data
		let data = Arc::new(Data {
			name: self.thread_name,
			stack_size: self.thread_stack_size,
			num_threads: AtomicUsize::new(threads),
			thread_count: AtomicUsize::new(0),
			injector,
			stealers: RwLock::new(stealers),
			shutdown: AtomicBool::new(false),
			parked_threads: ArrayQueue::new(threads),
			thread_handles: RwLock::new(Vec::new()),
		});
		// Use affinity if spawning thread per core
		if self.thread_per_core {
			// Spawn the desired number of workers
			for (id, worker) in workers.into_iter().enumerate() {
				Threadpool::spin_up(Some(id), data.clone(), worker, id);
			}
		} else {
			// Spawn the desired number of workers
			for (index, worker) in workers.into_iter().enumerate() {
				Threadpool::spin_up(None, data.clone(), worker, index);
			}
		}
		// Return the new threadpool
		Threadpool {
			data,
		}
	}
}
