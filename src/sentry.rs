use crate::Data;
use crate::Threadpool;
use std::sync::Weak;

pub(crate) struct Sentry {
	active: bool,
	coreid: Option<usize>,
	data: Weak<Data>,
}

impl Sentry {
	/// Create a new sentry tracker
	pub fn new(coreid: Option<usize>, data: Weak<Data>) -> Sentry {
		Sentry {
			data,
			coreid,
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
		let Some(data) = self.data.upgrade() else {
			return;
		};
		
		// If this sentry was still active,
		// then the task panicked without
		// properly cancelling the sentry,
		// so we should start a new thread.
		if self.active {
			// Spawn another new thread
			Threadpool::spin_up(self.coreid, data.clone());
		}
	}
}
