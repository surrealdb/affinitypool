//! Loom model for the parked-handshake protocol in [`crate::queue`].
//!
//! Loom exhaustively explores thread interleavings on a re-implementation
//! of the algorithm. The model below mirrors the production protocol in
//! `src/queue.rs` (the arm-then-rescan park dance, the `parked` counter
//! gate, the per-worker parker with its claim check, and the `shutdown`
//! sweep) using `loom::sync` primitives. The production code uses
//! lock-free `crossbeam_deque::Injector` for the actual queue storage,
//! which loom cannot instrument; the model replaces each shard's
//! `Injector<Runnable>` with an `AtomicUsize` item counter, and matches
//! the production code's [`std::sync::atomic::fence`]`(SeqCst)` between
//! queue access and `parked` access on both sides — the Dekker pattern
//! that closes the lost-wakeup race when the queue is lock-free.
//!
//! **MIRROR INVARIANT:** any change to `Queue::push`,
//! `Queue::pop_blocking`, `Queue::wake_one`, or `Queue::shutdown` in
//! `src/queue.rs` MUST be reflected in the corresponding method here,
//! and vice-versa. The whole point of this model is that it tests the
//! same handshake the production code relies on. Load-bearing details:
//!
//! * the `fence(SeqCst)` between the queue access and the `parked`
//!   access on each side;
//! * `parked` being a **counter**, so `parked > 0` holds across the
//!   whole of every worker's armed interval. A per-worker bitmask that
//!   other workers may clear is *not* a valid substitute: the Dekker
//!   argument needs "producer read 0" to imply "the arming worker's
//!   increment is later in the modification order", and a value another
//!   thread can clear does not give that;
//! * `wake_one` **claiming** a worker — testing `blocked` under that
//!   worker's own mutex and only counting the wake if it was still
//!   `true`. A `notify_one` to a condvar with no waiter is dropped, so
//!   counting an unclaimed candidate as the wake would strand a queued
//!   item while other workers slept.
//!
//! Intentionally **not** modelled, because none of them add a
//! happens-before edge the proof depends on: the pre-park spin/yield
//! rounds (they only re-run `try_pop`, which the model already explores
//! as Phase 1), the randomised victim start offset (it changes which
//! empty shard is probed first, not the ordering), and the self-spawn
//! fast path (producer and consumer are the same thread there, so there
//! is no inter-thread race to explore; its *peer wake* uses the very
//! same `push` tail modelled here).
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

/// Mirrors `crate::queue::WorkerParker`: the condvar a worker sleeps on
/// plus the in-mutex flag saying whether it is actually asleep.
struct WorkerParker {
	blocked: Mutex<bool>,
	cv: Condvar,
}

/// Sharded MPMC queue. Mirrors `crate::queue::Queue`, with each
/// shard's lock-free `Injector<Runnable>` replaced by an
/// `AtomicUsize` item counter and the actual `Runnable` payload
/// elided (items have no identity in the model — only their
/// presence/absence matters for the handshake proof).
struct Queue {
	shards: Box<[AtomicUsize]>,
	mask: usize,
	parkers: Box<[WorkerParker]>,
	parked: AtomicUsize,
	shutdown: AtomicBool,
}

impl Queue {
	fn new(num_shards: usize, num_workers: usize) -> Self {
		assert!(num_shards.is_power_of_two() && num_shards >= 1);
		let shards: Vec<AtomicUsize> = (0..num_shards).map(|_| AtomicUsize::new(0)).collect();
		let parkers: Vec<WorkerParker> = (0..num_workers)
			.map(|_| WorkerParker {
				blocked: Mutex::new(false),
				cv: Condvar::new(),
			})
			.collect();
		Self {
			shards: shards.into_boxed_slice(),
			mask: num_shards - 1,
			parkers: parkers.into_boxed_slice(),
			parked: AtomicUsize::new(0),
			shutdown: AtomicBool::new(false),
		}
	}

	/// Mirrors `Queue::push` in `src/queue.rs`. The shard-hint lookup
	/// and spill counter are replaced by an explicit `shard_hint` so
	/// the model controls routing deterministically. The
	/// `fence(SeqCst)` between the push and the `parked.load` mirrors
	/// the production fence — load-bearing for the lost-wakeup proof.
	fn push(&self, shard_hint: usize) {
		let idx = shard_hint & self.mask;
		self.shards[idx].fetch_add(1, Ordering::Release);
		fence(Ordering::SeqCst);
		if self.parked.load(Ordering::Acquire) > 0 {
			self.wake_one(idx);
		}
	}

	/// Mirrors `Queue::wake_one` in `src/queue.rs`: walk candidates
	/// from `hint`, claim the first worker still inside `cv.wait`
	/// under its own mutex, and wake exactly that one.
	fn wake_one(&self, hint: usize) -> bool {
		let n = self.parkers.len();
		let start = hint % n;
		// Pass 1, non-blocking.
		let mut contended = false;
		for k in 0..n {
			let parker = &self.parkers[(start + k) % n];
			match parker.blocked.try_lock() {
				Ok(mut blocked) => {
					if *blocked {
						*blocked = false;
						parker.cv.notify_one();
						return true;
					}
				}
				Err(_) => contended = true,
			}
		}
		if !contended {
			return false;
		}
		// Pass 2, blocking over the slots we could not inspect: a
		// worker holding its own lock may be mid re-scan and about to
		// sleep, so "busy" must not be read as "it will find it".
		for k in 0..n {
			let parker = &self.parkers[(start + k) % n];
			let mut blocked = parker.blocked.lock().unwrap();
			if *blocked {
				*blocked = false;
				parker.cv.notify_one();
				return true;
			}
		}
		false
	}

	/// Try one lock-free scan pass. Mirrors `Queue::scan` in
	/// `src/queue.rs`: walks shards starting from `worker_idx &
	/// mask`, returns the first non-empty one. Acquire on both load
	/// and CAS — Release/Acquire is enough for coherence of the shard
	/// counter; the cross-thread edge for the lost-wakeup proof comes
	/// from the `fence(SeqCst)` in `pop_blocking`.
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
	/// `fence(SeqCst)` between `parked.fetch_add` and the re-scan
	/// pairs with the producer's fence to form the Dekker invariant.
	fn pop_blocking(&self, worker_idx: usize) -> Option<()> {
		loop {
			// Phase 1: lock-free scan.
			if let Some(r) = self.try_pop(worker_idx) {
				return Some(r);
			}

			// Phase 2: arm parking. Take our own parker's lock, bump
			// the shared counter BEFORE the re-scan, then
			// SeqCst-fence so the re-scan and any producer's
			// load-after-push are totally ordered.
			let mut blocked = self.parkers[worker_idx].blocked.lock().unwrap();
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

			*blocked = true;
			blocked = self.parkers[worker_idx].cv.wait(blocked).unwrap();
			*blocked = false;
			self.parked.fetch_sub(1, Ordering::Release);
			drop(blocked);
		}
	}

	/// Mirrors `Queue::shutdown` in `src/queue.rs`: visit every
	/// parker, since each worker sleeps on its own condvar.
	fn shutdown(&self) {
		self.shutdown.store(true, Ordering::Release);
		for parker in &*self.parkers {
			let mut blocked = parker.blocked.lock().unwrap();
			*blocked = false;
			parker.cv.notify_one();
		}
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
		let q = Arc::new(Queue::new(2, 1));
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
		let q = Arc::new(Queue::new(2, 1));
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

/// Two workers, one item, then shutdown.
///
/// This is the test that covers the *targeted* wake and its claim
/// check. The producer's wake hint is the shard it pushed to, so
/// `wake_one` starts at one specific worker; across interleavings loom
/// drives every combination of which workers are armed, in `cv.wait`,
/// or already gone by the time the wake walks them — including the
/// cases where the hinted worker cannot be claimed and the walk must
/// fall through to the other one.
///
/// Asserting *exactly one* `Some` catches both failure modes at once: a
/// lost item (nobody gets it) and a double-take (both do). Neither
/// worker may hang — loom fails the model if a thread cannot finish —
/// which is what would happen if a wake were counted but dropped.
#[test]
fn one_push_two_workers_exactly_one_wins() {
	loom::model(|| {
		let q = Arc::new(Queue::new(2, 2));
		let producer = {
			let q = q.clone();
			thread::spawn(move || {
				q.push(0);
				// Let the loser exit rather than sleep forever.
				q.shutdown();
			})
		};
		let w0 = {
			let q = q.clone();
			thread::spawn(move || q.pop_blocking(0))
		};
		let w1 = {
			let q = q.clone();
			thread::spawn(move || q.pop_blocking(1))
		};
		producer.join().unwrap();
		let a = w0.join().unwrap();
		let b = w1.join().unwrap();
		let winners = a.iter().count() + b.iter().count();
		assert_eq!(winners, 1, "exactly one worker must take the single item");
	});
}

/// Two items, two workers, **no shutdown rescue**: both workers must
/// come back with an item.
///
/// This is the test that pins `wake_one`'s claim check. With two items
/// queued, every worker that parks has work waiting for it, so a wake
/// that is *counted but dropped* — `notify_one` to a condvar whose
/// worker has already left `cv.wait` — leaves one worker asleep with an
/// item still in the queue and nothing left to wake it. There is no
/// `shutdown()` here precisely because a shutdown would rescue that
/// worker and hide the bug; instead loom's deadlock detection fires on
/// the permanently blocked thread.
///
/// Verified to fail if `wake_one` is changed to notify its first
/// candidate unconditionally instead of claiming a worker whose
/// `blocked` flag is still set.
#[test]
fn two_pushes_two_workers_neither_is_stranded() {
	loom::model(|| {
		let q = Arc::new(Queue::new(2, 2));
		let producer = {
			let q = q.clone();
			thread::spawn(move || {
				q.push(0);
				q.push(1);
			})
		};
		let w0 = {
			let q = q.clone();
			thread::spawn(move || q.pop_blocking(0))
		};
		let w1 = {
			let q = q.clone();
			thread::spawn(move || q.pop_blocking(1))
		};
		producer.join().unwrap();
		assert_eq!(w0.join().unwrap(), Some(()));
		assert_eq!(w1.join().unwrap(), Some(()));
	});
}

/// `shutdown()` must wake a worker that is already parked (or
/// about to park) on an empty queue. Without taking the worker's
/// own lock in `shutdown`, the notify can race a worker mid-arm and
/// the worker hangs.
#[test]
fn shutdown_wakes_parked_worker() {
	loom::model(|| {
		let q = Arc::new(Queue::new(2, 1));
		let worker = {
			let q = q.clone();
			thread::spawn(move || q.pop_blocking(0))
		};
		q.shutdown();
		let got = worker.join().unwrap();
		assert_eq!(got, None);
	});
}

/// `shutdown()` must wake *every* parked worker, not just one —
/// each sleeps on its own condvar, so a single notify would leave
/// the others hanging and `Threadpool::drop` would block joining them.
#[test]
fn shutdown_wakes_all_parked_workers() {
	loom::model(|| {
		let q = Arc::new(Queue::new(2, 2));
		let w0 = {
			let q = q.clone();
			thread::spawn(move || q.pop_blocking(0))
		};
		let w1 = {
			let q = q.clone();
			thread::spawn(move || q.pop_blocking(1))
		};
		q.shutdown();
		assert_eq!(w0.join().unwrap(), None);
		assert_eq!(w1.join().unwrap(), None);
	});
}

/// Shutdown signalled while an item is still queued: the worker
/// drains the item before observing the shutdown flag. Validates
/// that the shutdown check happens *after* the re-scan, not
/// before.
#[test]
fn shutdown_drains_pending_item() {
	loom::model(|| {
		let q = Arc::new(Queue::new(2, 1));
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
