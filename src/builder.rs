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
	/// One worker per core, each pinned to its core. Only settable via
	/// [`Builder::with_affinity_pinning`] (the `pinning` feature); stays
	/// `false` otherwise, so the default build never pins.
	affinity_pinning: bool,
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
			affinity_pinning: false,
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

	/// Spawn one worker thread per CPU core and pin each worker to its
	/// core. Requires the `pinning` crate feature.
	///
	/// Pinning is a best-effort hint: on platforms or hardware that do
	/// not support thread affinity (e.g. Apple Silicon) the workers are
	/// still spawned one-per-core but the pin is a no-op. Pinning helps
	/// on bare-metal NUMA / latency-sensitive deployments; it is rarely
	/// beneficial (and sometimes harmful) under containers or VMs where
	/// "cores" are virtualised.
	///
	/// # Examples
	///
	/// Each thread spawned will be linked to a separate core:
	///
	/// ```
	/// let pool = affinitypool::Builder::new()
	///     .with_affinity_pinning(true)
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
	#[cfg(feature = "pinning")]
	pub fn with_affinity_pinning(mut self, enabled: bool) -> Builder {
		self.affinity_pinning = enabled;
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
		} else if self.affinity_pinning {
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
			queue: Arc::new(Queue::new(threads)),
			thread_handles: Mutex::new(Vec::new()),
		});
		// Spawn the desired number of workers.
		if self.affinity_pinning {
			for index in 0..threads {
				Threadpool::spin_up(Some(index), data.clone(), index);
			}
		} else {
			for index in 0..threads {
				Threadpool::spin_up(None, data.clone(), index);
			}
		}
		// Return the new threadpool.
		Threadpool {
			data,
		}
	}
}
