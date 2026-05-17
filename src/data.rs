use crate::task::OwnedTask;
use arc_swap::ArcSwap;
use crossbeam::deque::{Injector, Stealer};
use crossbeam::queue::ArrayQueue;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::thread::{JoinHandle, Thread};

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
	/// Stealers for all worker threads.
	///
	/// Stored as a boxed slice behind `ArcSwap` so the work-stealing loop
	/// observes the current slice via a single atomic load instead of taking
	/// an `RwLock` read on every retry. Writers (sentry respawns) build a
	/// fresh slice and atomically swap it in.
	pub(crate) stealers: ArcSwap<Box<[Stealer<OwnedTask<'static>>]>>,
	/// Serialises concurrent stealer-slice rebuilds so two simultaneous
	/// respawns can't both clone the old slice and race a final store. The
	/// hot path does not touch this lock.
	pub(crate) stealers_lock: Mutex<()>,
	/// Queue of parked threads waiting for work
	pub(crate) parked_threads: ArrayQueue<Thread>,
	/// Flag to indicate if workers should shut down
	pub(crate) shutdown: AtomicBool,
	/// Handles to all worker threads for cleanup. Only touched on
	/// spawn/respawn (cold path) and on `Drop`.
	pub(crate) thread_handles: Mutex<Vec<JoinHandle<()>>>,
}
