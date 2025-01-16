use crate::Data;
use crate::Threadpool;
use std::sync::Arc;

pub(crate) struct Sentry<'a> {
	active: bool,
	coreid: Option<usize>,
	data: &'a Arc<Data>,
}

impl<'a> Sentry<'a> {
	/// Create a new sentry tracker
	pub fn new(coreid: Option<usize>, data: &'a Arc<Data>) -> Sentry<'a> {
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

impl Drop for Sentry<'_> {
	fn drop(&mut self) {
		// If this sentry was still active,
		// then the task panicked without
		// properly cancelling the sentry,
		// so we should start a new thread.
		if self.active {
			// Spawn another new thread
			Threadpool::spin_up(self.coreid, self.data.clone());
		}
	}
}
