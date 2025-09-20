use crate::task::OwnedTask;
use crossbeam::deque::{Injector, Stealer};
use crossbeam::queue::ArrayQueue;
use parking_lot::RwLock;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::thread::Thread;

/// Data shared between all worker threads
pub(crate) struct Data {
	/// The name of each thread
	pub(crate) name: Option<String>,
	/// The stack size for each thread
	pub(crate) stack_size: Option<usize>,
	/// The specified number of threads
	pub(crate) num_threads: AtomicUsize,
	/// The current number of threads
	pub(crate) thread_count: AtomicUsize,
	/// The global task queue (injector)
	pub(crate) injector: Injector<OwnedTask<'static>>,
	/// Stealers for all worker threads
	pub(crate) stealers: RwLock<Vec<Stealer<OwnedTask<'static>>>>,
	/// Flag to indicate if workers should shut down
	pub(crate) shutdown: AtomicBool,
	/// Queue of parked threads waiting for work
	pub(crate) parked_threads: ArrayQueue<Thread>,
}
