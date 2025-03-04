use crate::Data;
use crate::Threadpool;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

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
	/// let builder = threadpool::Builder::new();
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
	/// let pool = threadpool::Builder::new()
	///     .worker_threads(8)
	///     .build();
	///
	/// for _ in 0..100 {
	///     pool.execute(|| {
	///         println!("Hello from a worker thread!")
	///     })
	/// }
	/// ```
	pub fn worker_threads(mut self, num_threads: usize) -> Builder {
		assert!(num_threads > 0);
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
	/// let pool = threadpool::Builder::new()
	///     .thread_name("foo".into())
	///     .build();
	///
	/// for _ in 0..100 {
	///     pool.execute(|| {
	///         assert_eq!(thread::current().name(), Some("foo"));
	///     })
	/// }
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
	/// let pool = threadpool::Builder::new()
	///     .thread_stack_size(4_000_000)
	///     .build();
	///
	/// for _ in 0..100 {
	///     pool.execute(|| {
	///         println!("This thread has a 4 MB stack size!");
	///     })
	/// }
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
	/// let pool = threadpool::Builder::new()
	///     .thread_per_core(true)
	///     .build();
	///
	/// for _ in 0..100 {
	///     pool.execute(|| {
	///         println!("This is executed on individual cores!");
	///     })
	/// }
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
	/// let pool = threadpool::Builder::new()
	///     .worker_threads(8)
	///     .thread_stack_size(4_000_000)
	///     .build();
	/// ```
	pub fn build(self) -> Threadpool {
		// Create a queuing channel for tasks
		let (send, recv) = async_channel::unbounded();
		// Calculate how many threads to spawn
		let workers = if self.thread_per_core || self.num_threads.is_none() {
			num_cpus::get()
		} else {
			self.num_threads.unwrap()
		};
		// Create the threadpool shared data
		let data = Arc::new(Data {
			name: self.thread_name,
			stack_size: None,
			num_threads: AtomicUsize::new(workers),
			thread_count: AtomicUsize::new(0),
			sender: send,
			receiver: recv,
		});
		// Use affinity if spawning thread per core
		if self.thread_per_core {
			// Spawn the desired number of workers
			for id in 0..workers {
				Threadpool::spin_up(Some(id), data.clone());
			}
		} else {
			// Spawn the desired number of workers
			for _ in 0..workers {
				Threadpool::spin_up(None, data.clone());
			}
		}
		// Return the new threadpool
		Threadpool {
			data,
		}
	}
}
