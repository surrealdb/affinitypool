use crate::Data;
use crate::MAX_THREADS;
use crate::Threadpool;
use crate::queue::Queue;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

#[derive(Default, Clone)]
pub struct Builder {
	num_threads: Option<usize>,
	thread_name: Option<String>,
	thread_stack_size: Option<usize>,
	thread_per_core: bool,
	shards: Option<usize>,
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
			shards: None,
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

	/// Spawn one worker thread per CPU core.
	///
	/// This sets the worker count to the number of available cores. It
	/// does **not** pin threads to cores — worker placement is left to
	/// the OS scheduler. (Earlier versions also pinned each worker via
	/// platform affinity APIs; that was removed — see the changelog.)
	///
	/// # Examples
	///
	/// ```
	/// let pool = affinitypool::Builder::new()
	///     .thread_per_core(true)
	///     .build();
	///
	/// # tokio::runtime::Runtime::new().unwrap().block_on(async {
	///     for _ in 0..10 {
	///         pool.spawn(|| {
	///             println!("Running on a one-worker-per-core pool!");
	///         }).await;
	///     }
	/// # });
	/// ```
	pub fn thread_per_core(mut self, enabled: bool) -> Builder {
		self.thread_per_core = enabled;
		self
	}

	/// Set how many queue shards producers distribute work across.
	///
	/// Each producer thread sticks to one shard, so more shards means
	/// less contention between concurrent producers but more empty
	/// queues for an idle worker to scan. The default is one shard per
	/// worker capped at 8, which suits most pools; raise it for a pool
	/// fed by many concurrent producers on a high-core machine.
	///
	/// The value is clamped to `1..=worker_threads` and then rounded up
	/// to a power of two, so the effective count can exceed the worker
	/// count when that count is not itself a power of two (8 workers
	/// with `shards(5)` gives 8 shards; 3 workers gives at most 4).
	///
	/// # Examples
	///
	/// ```
	/// let pool = affinitypool::Builder::new()
	///     .worker_threads(32)
	///     .shards(16)
	///     .build();
	/// ```
	pub fn shards(mut self, shards: usize) -> Builder {
		self.shards = Some(shards);
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
		// Calculate how many threads to spawn.
		let threads = if let Some(num_threads) = self.num_threads {
			num_threads.clamp(1, MAX_THREADS)
		} else if self.thread_per_core {
			num_cpus::get().clamp(1, MAX_THREADS)
		} else {
			2
		};
		// Create the threadpool shared data.
		let data = Arc::new(Data {
			name: self.thread_name,
			stack_size: self.thread_stack_size,
			num_threads: AtomicUsize::new(threads),
			thread_count: AtomicUsize::new(0),
			queue: Arc::new(Queue::new(threads, self.shards)),
			thread_handles: Mutex::new(Vec::new()),
		});
		// Spawn the desired number of workers.
		for index in 0..threads {
			Threadpool::spin_up(data.clone(), index);
		}
		// Return the new threadpool.
		Threadpool {
			data,
		}
	}
}
