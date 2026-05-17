use crate::Data;
use crate::Threadpool;
use crossbeam::deque::Worker;
use std::sync::{Arc, Weak};

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
			// Create a new worker for the replacement thread.
			let worker = Worker::new_fifo();
			let new_stealer = worker.stealer();
			// Serialise stealer-slice rebuilds so concurrent respawns can't
			// race; build a fresh slice with the dead worker's slot replaced
			// and atomically swap it in.
			{
				let _guard = data.stealers_lock.lock();
				let current = data.stealers.load_full();
				let mut next: Vec<_> = current.iter().cloned().collect();
				next[self.index] = new_stealer;
				data.stealers.store(Arc::new(next.into_boxed_slice()));
			}
			// Spawn another new thread
			Threadpool::spin_up(self.coreid, data.clone(), worker, self.index);
		}
	}
}
