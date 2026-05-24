//! Loom model for the parked-handshake protocol in [`crate::queue`].
//!
//! Loom exhaustively explores thread interleavings on a re-implementation
//! of the algorithm. The model below mirrors the production protocol in
//! `src/queue.rs` (the arm-then-rescan park dance, the `parked` atomic
//! gate, and the `shutdown` broadcast) using `loom::sync` primitives and
//! a `u32` stand-in for `async_task::Runnable`. The production code uses
//! `parking_lot` + `std::sync::atomic` which loom cannot instrument, so
//! the model is intentionally a copy.
//!
//! **MIRROR INVARIANT:** any change to `Queue::push`, `Queue::pop_blocking`,
//! or `Queue::shutdown` in `src/queue.rs` MUST be reflected in the
//! corresponding method here, and vice-versa. The whole point of this
//! model is that it tests the same handshake the production code
//! relies on.
//!
//! Run with:
//!
//! ```sh
//! RUSTFLAGS="--cfg loom" cargo test --release --test loom_queue
//! ```

#![cfg(loom)]

use loom::sync::Arc;
use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use loom::sync::{Condvar, Mutex};
use loom::thread;
use std::collections::VecDeque;

/// Sharded MPMC queue. Mirrors `crate::queue::Queue` but with `u32`
/// items and loom synchronisation primitives. See module docs for the
/// mirror invariant.
struct Queue {
	shards: Box<[Mutex<VecDeque<u32>>]>,
	mask: usize,
	park: Mutex<()>,
	notify: Condvar,
	parked: AtomicUsize,
	shutdown: AtomicBool,
}

impl Queue {
	fn new(num_shards: usize) -> Self {
		assert!(num_shards.is_power_of_two() && num_shards >= 1);
		let shards: Vec<Mutex<VecDeque<u32>>> =
			(0..num_shards).map(|_| Mutex::new(VecDeque::new())).collect();
		Self {
			shards: shards.into_boxed_slice(),
			mask: num_shards - 1,
			park: Mutex::new(()),
			notify: Condvar::new(),
			parked: AtomicUsize::new(0),
			shutdown: AtomicBool::new(false),
		}
	}

	/// Mirrors `Queue::push` in `src/queue.rs`. The CPU lookup is
	/// replaced by an explicit `shard_hint` so the model controls
	/// routing deterministically.
	fn push(&self, shard_hint: usize, item: u32) {
		let idx = shard_hint & self.mask;
		self.shards[idx].lock().unwrap().push_back(item);
		if self.parked.load(Ordering::Acquire) > 0 {
			let _g = self.park.lock().unwrap();
			self.notify.notify_one();
		}
	}

	/// Mirrors `Queue::pop_blocking` in `src/queue.rs`.
	fn pop_blocking(&self, worker_idx: usize) -> Option<u32> {
		let n = self.mask + 1;
		let my_shard = worker_idx & self.mask;

		loop {
			// Phase 1: lock-free scan.
			for offset in 0..n {
				let idx = (my_shard + offset) & self.mask;
				if let Some(r) = self.shards[idx].lock().unwrap().pop_front() {
					return Some(r);
				}
			}

			// Phase 2: arm parking. Acquire park, bump `parked`
			// BEFORE the re-scan.
			let mut park = self.park.lock().unwrap();
			self.parked.fetch_add(1, Ordering::Release);

			let mut found = None;
			for offset in 0..n {
				let idx = (my_shard + offset) & self.mask;
				if let Some(r) = self.shards[idx].lock().unwrap().pop_front() {
					found = Some(r);
					break;
				}
			}
			if let Some(r) = found {
				self.parked.fetch_sub(1, Ordering::Release);
				return Some(r);
			}

			if self.shutdown.load(Ordering::Acquire) {
				self.parked.fetch_sub(1, Ordering::Release);
				return None;
			}

			park = self.notify.wait(park).unwrap();
			self.parked.fetch_sub(1, Ordering::Release);
			drop(park);
		}
	}

	/// Mirrors `Queue::shutdown` in `src/queue.rs`.
	fn shutdown(&self) {
		self.shutdown.store(true, Ordering::Release);
		let _g = self.park.lock().unwrap();
		self.notify.notify_all();
	}
}

/// One producer, one worker, single push, two shards.
///
/// Validates the core lost-wakeup invariant: regardless of how the
/// producer's push interleaves with the worker's Phase-1 scan, Phase-2
/// arm, or `cv.wait`, the worker must always observe the item.
#[test]
fn one_push_one_worker_two_shards() {
	loom::model(|| {
		let q = Arc::new(Queue::new(2));
		let producer = {
			let q = q.clone();
			thread::spawn(move || q.push(0, 1))
		};
		let worker = {
			let q = q.clone();
			thread::spawn(move || q.pop_blocking(0))
		};
		producer.join().unwrap();
		let got = worker.join().unwrap();
		assert_eq!(got, Some(1));
	});
}

/// Producer pushes to a *different* shard than the worker's preferred
/// shard. Worker must still observe the push through its scan phase
/// (Phase 1 or Phase 2).
#[test]
fn push_to_remote_shard() {
	loom::model(|| {
		let q = Arc::new(Queue::new(2));
		let producer = {
			let q = q.clone();
			thread::spawn(move || q.push(1, 7))
		};
		let worker = {
			let q = q.clone();
			thread::spawn(move || q.pop_blocking(0))
		};
		producer.join().unwrap();
		let got = worker.join().unwrap();
		assert_eq!(got, Some(7));
	});
}

/// `shutdown()` must wake a worker that is already parked (or about to
/// park) on an empty queue. Without the `park.lock()` in `shutdown`,
/// the broadcast can race a worker mid-arm and the worker hangs.
#[test]
fn shutdown_wakes_parked_worker() {
	loom::model(|| {
		let q = Arc::new(Queue::new(2));
		let worker = {
			let q = q.clone();
			thread::spawn(move || q.pop_blocking(0))
		};
		q.shutdown();
		let got = worker.join().unwrap();
		assert_eq!(got, None);
	});
}

/// Shutdown signalled while an item is still queued: the worker drains
/// the item before observing the shutdown flag. Validates that the
/// shutdown check happens *after* the re-scan, not before.
#[test]
fn shutdown_drains_pending_item() {
	loom::model(|| {
		let q = Arc::new(Queue::new(2));
		// Pre-seed before launching threads — no race on the push
		// itself, only on shutdown vs the worker's pop.
		q.push(0, 9);
		let worker = {
			let q = q.clone();
			thread::spawn(move || q.pop_blocking(0))
		};
		q.shutdown();
		let got = worker.join().unwrap();
		assert_eq!(got, Some(9));
	});
}
