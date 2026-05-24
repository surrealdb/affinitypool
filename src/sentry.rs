use crate::Data;
use crate::Threadpool;
use std::sync::Weak;

/// Death-watch guardian for worker threads.
///
/// If a worker thread unwinds (panic outside `Runnable::run`, since
/// `async-task` catches panics inside the future), the sentry's `Drop`
/// fires and spins up a replacement worker. Panics inside the future
/// itself never reach the sentry — async-task stores them in the task
/// header for the awaiter — so under normal operation this path is
/// dead.
pub(crate) struct Sentry {
	active: bool,
	coreid: Option<usize>,
	index: usize,
	data: Weak<Data>,
}

impl Sentry {
	/// Create a new sentry tracker.
	pub fn new(coreid: Option<usize>, index: usize, data: Weak<Data>) -> Sentry {
		Sentry {
			data,
			coreid,
			index,
			active: true,
		}
	}

	/// Cancel and destroy this sentry. Called on the worker's clean
	/// shutdown path so `Drop` doesn't trigger a respawn.
	pub fn cancel(mut self) {
		self.active = false;
	}
}

impl Drop for Sentry {
	fn drop(&mut self) {
		// Upgrade the weak reference to an Arc.
		let Some(data) = self.data.upgrade() else {
			return;
		};
		// If this sentry was still active, the worker panicked without
		// properly cancelling the sentry, so we start a replacement.
		if self.active {
			Threadpool::spin_up(self.coreid, data, self.index);
		}
	}
}
