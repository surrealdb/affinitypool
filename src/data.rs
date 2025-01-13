use async_channel::{Receiver, Sender};
use std::sync::atomic::AtomicUsize;

use crate::task::OwnedTask;

#[derive(Debug)]
pub(crate) struct Data {
	/// The name of each thread
	pub(crate) name: Option<String>,
	/// The stack size for each thread
	pub(crate) stack_size: Option<usize>,
	/// The specified number of threads
	pub(crate) num_threads: AtomicUsize,
	/// The current number of threads
	pub(crate) thread_count: AtomicUsize,
	/// The sender used for queueing jobs for processing
	pub(crate) sender: Sender<OwnedTask<'static>>,
	/// The receiver used for taking jobs to be processed
	pub(crate) receiver: Receiver<OwnedTask<'static>>,
}
