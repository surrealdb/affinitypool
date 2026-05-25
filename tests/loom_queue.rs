//! Loom model for the parked-handshake protocol in [`crate::queue`].
//!
//! Loom exhaustively explores thread interleavings on a re-implementation
//! of the algorithm. The model below mirrors the production protocol in
//! `src/queue.rs` (the arm-then-rescan park dance, the `parked` atomic
//! gate, and the `shutdown` broadcast) using `loom::sync` primitives.
//! The production code uses lock-free `crossbeam_deque::Injector` for
//! the actual queue storage, which loom cannot instrument; the model
//! replaces each shard's `Injector<Runnable>` with an `AtomicUsize`
//! item counter, and matches the production code's
//! [`std::sync::atomic::fence`]`(SeqCst)` between queue access and
//! `parked` access on both sides — the Dekker pattern that closes
//! the lost-wakeup race when the queue is lock-free.
//!
//! **MIRROR INVARIANT:** any change to `Queue::push`, `Queue::pop_blocking`,
//! or `Queue::shutdown` in `src/queue.rs` MUST be reflected in the
//! corresponding method here, and vice-versa. The whole point of this
//! model is that it tests the same handshake the production code
//! relies on. In particular, the `fence(SeqCst)` between the queue
//! access and the `parked` access on each side is load-bearing for
//! the lost-wakeup proof.
//!
//! Run with:
//!
//! ```sh
//! RUSTFLAGS="--cfg loom" cargo test --release --test loom_queue
//! ```

#![cfg(loom)]

use loom::sync::Arc;
use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering, fence};
use loom::sync::{Condvar, Mutex};
use loom::thread;

/// Sharded MPMC queue. Mirrors `crate::queue::Queue`, with each
/// shard's lock-free `Injector<Runnable>` replaced by an
/// `AtomicUsize` item counter and the actual `Runnable` payload
/// elided (items have no identity in the model — only their
/// presence/absence matters for the handshake proof).
///
/// Push and pop on the shard counters use Release/Acquire (the
/// minimum needed to make the counter coherent across threads);
/// the cross-thread happens-before edge that closes the
/// lost-wakeup race comes from the SeqCst fence between the
/// queue access and the `parked` access on each side.
struct Queue {
	shards: Box<[AtomicUsize]>,
	mask: usize,
	park: Mutex<()>,
	notify: Condvar,
	parked: AtomicUsize,
	shutdown: AtomicBool,
}

impl Queue {
	fn new(num_shards: usize) -> Self {
		assert!(num_shards.is_power_of_two() && num_shards >= 1);
		let shards: Vec<AtomicUsize> = (0..num_shards).map(|_| AtomicUsize::new(0)).collect();
		Self {
			shards: shards.into_boxed_slice(),
			mask: num_shards - 1,
			park: Mutex::new(()),
			notify: Condvar::new(),
			parked: AtomicUsize::new(0),
			shutdown: AtomicBool::new(false),
		}
	}

	/// Mirrors `Queue::push` in `src/queue.rs`. The CPU lookup
	/// and spill counter are replaced by an explicit `shard_hint`
	/// so the model controls routing deterministically. The
	/// `fence(SeqCst)` between the push and the `parked.load`
	/// mirrors the production fence — load-bearing for the
	/// lost-wakeup proof.
	fn push(&self, shard_hint: usize) {
		let idx = shard_hint & self.mask;
		self.shards[idx].fetch_add(1, Ordering::Release);
		fence(Ordering::SeqCst);
		if self.parked.load(Ordering::Acquire) > 0 {
			let _g = self.park.lock().unwrap();
			self.notify.notify_one();
		}
	}

	/// Try one lock-free scan pass. Mirrors `Queue::scan` in
	/// `src/queue.rs`: walks shards starting from `worker_idx &
	/// mask` in cyclic order, returns the first non-empty one.
	/// Acquire on both load and CAS — Release/Acquire is enough
	/// for coherence of the shard counter; the cross-thread
	/// edge for the lost-wakeup proof comes from the `fence(SeqCst)`
	/// in `pop_blocking`.
	fn try_pop(&self, worker_idx: usize) -> Option<()> {
		let n = self.mask + 1;
		let my_shard = worker_idx & self.mask;
		for offset in 0..n {
			let idx = (my_shard + offset) & self.mask;
			let mut current = self.shards[idx].load(Ordering::Acquire);
			while current > 0 {
				match self.shards[idx].compare_exchange(
					current,
					current - 1,
					Ordering::AcqRel,
					Ordering::Acquire,
				) {
					Ok(_) => return Some(()),
					Err(c) => current = c,
				}
			}
		}
		None
	}

	/// Mirrors `Queue::pop_blocking` in `src/queue.rs`. The
	/// `fence(SeqCst)` between `parked.fetch_add` and the
	/// re-scan pairs with the producer's fence to form the
	/// Dekker invariant.
	fn pop_blocking(&self, worker_idx: usize) -> Option<()> {
		loop {
			// Phase 1: lock-free scan.
			if let Some(r) = self.try_pop(worker_idx) {
				return Some(r);
			}

			// Phase 2: arm parking. Acquire park, bump parked
			// BEFORE the re-scan, then SeqCst-fence so the
			// re-scan and any producer's load-after-push are
			// totally ordered.
			let mut park = self.park.lock().unwrap();
			self.parked.fetch_add(1, Ordering::Release);
			fence(Ordering::SeqCst);

			if let Some(r) = self.try_pop(worker_idx) {
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
/// producer's push interleaves with the worker's Phase-1 scan,
/// Phase-2 arm, or `cv.wait`, the worker must always observe the
/// item.
#[test]
fn one_push_one_worker_two_shards() {
	loom::model(|| {
		let q = Arc::new(Queue::new(2));
		let producer = {
			let q = q.clone();
			thread::spawn(move || q.push(0))
		};
		let worker = {
			let q = q.clone();
			thread::spawn(move || q.pop_blocking(0))
		};
		producer.join().unwrap();
		let got = worker.join().unwrap();
		assert_eq!(got, Some(()));
	});
}

/// Producer pushes to a *different* shard than the worker's
/// preferred shard. Worker must still observe the push through
/// its scan phase (Phase 1 or Phase 2).
#[test]
fn push_to_remote_shard() {
	loom::model(|| {
		let q = Arc::new(Queue::new(2));
		let producer = {
			let q = q.clone();
			thread::spawn(move || q.push(1))
		};
		let worker = {
			let q = q.clone();
			thread::spawn(move || q.pop_blocking(0))
		};
		producer.join().unwrap();
		let got = worker.join().unwrap();
		assert_eq!(got, Some(()));
	});
}

/// `shutdown()` must wake a worker that is already parked (or
/// about to park) on an empty queue. Without the `park.lock()` in
/// `shutdown`, the broadcast can race a worker mid-arm and the
/// worker hangs.
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

/// Shutdown signalled while an item is still queued: the worker
/// drains the item before observing the shutdown flag. Validates
/// that the shutdown check happens *after* the re-scan, not
/// before.
#[test]
fn shutdown_drains_pending_item() {
	loom::model(|| {
		let q = Arc::new(Queue::new(2));
		// Pre-seed before launching threads — no race on the push
		// itself, only on shutdown vs the worker's pop.
		q.push(0);
		let worker = {
			let q = q.clone();
			thread::spawn(move || q.pop_blocking(0))
		};
		q.shutdown();
		let got = worker.join().unwrap();
		assert_eq!(got, Some(()));
	});
}
