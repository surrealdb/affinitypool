use crate::Data;
use crate::Threadpool;
use crossbeam::deque::Worker;
use std::sync::Weak;

pub(crate) struct Sentry {
	active: bool,
	coreid: Option<usize>,
	index: usize,
	data: Weak<Data>,
}

impl Sentry {
	/// Create a new sentry tracker
	pub fn new(coreid: Option<usize>, index: usize, data: Weak<Data>) -> Sentry {
		Sentry {
			data,
			coreid,
			index,
			active: true,
		}
	}
	/// Cancel and destroy this sentry
	pub fn cancel(mut self) {
		self.active = false;
	}
}

impl Drop for Sentry {
	fn drop(&mut self) {
		// Upgrade the weak reference to an Arc
		let Some(data) = self.data.upgrade() else {
			return;
		};
		// If this sentry was still active, then the task panicked without
		// properly cancelling the sentry, so we should start a new thread.
		if self.active {
			// Create a new worker for the replacement thread
			let worker = Worker::new_fifo();
			// Replace the old stealer with the new one
			{
				let mut stealers = data.stealers.write();
				stealers[self.index] = worker.stealer();
			}
			// Spawn another new thread
			Threadpool::spin_up(self.coreid, data.clone(), worker, self.index);
		}
	}
}
