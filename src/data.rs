use crate::queue::Queue;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::thread::JoinHandle;

/// Data shared between all worker threads.
pub(crate) struct Data {
	/// The name of each thread.
	pub(crate) name: Option<String>,
	/// The stack size for each thread.
	pub(crate) stack_size: Option<usize>,
	/// The specified number of threads.
	pub(crate) num_threads: AtomicUsize,
	/// The current number of threads.
	pub(crate) thread_count: AtomicUsize,
	/// The shared work queue. Producers push `async_task::Runnable`s
	/// (from `spawn` and `spawn_local`); workers pop and run them. The
	/// queue's own `Condvar` handles parking when idle, and its
	/// `AtomicBool` shutdown flag is set on `Threadpool::drop`.
	pub(crate) queue: Arc<Queue>,
	/// Handles to all worker threads for cleanup. Only touched on
	/// spawn/respawn (cold path) and on `Drop`.
	pub(crate) thread_handles: Mutex<Vec<JoinHandle<()>>>,
}
