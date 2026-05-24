//! Sharded MPMC queue used by `Threadpool` to deliver `Runnable`s from
//! producers to worker threads.
//!
//! ## Architecture
//!
//! Up to [`MAX_SHARDS`] independent `parking_lot::Mutex<VecDeque<Runnable>>`
//! shards, plus a single shared `Condvar` for parking. Producers pick
//! a shard via the thread-local CPU cache (see [`crate::cpu`]) — a
//! producer running on core N consistently lands on shard `N & mask`,
//! which co-locates closures with the worker pinned to a nearby core
//! and keeps each producer-thread's pushes off the other producers'
//! shards.
//!
//! Each worker has a *preferred* shard equal to `worker_idx & mask`.
//! It pops from its own shard first; if empty, it scans the remaining
//! shards in cyclic order before parking. This is shard-scanning, not
//! work-stealing — there is no separate notion of victim/thief
//! queues and no per-task steal-retry spin.
//!
//! Number of shards is `num_workers.next_power_of_two().min(MAX_SHARDS)`,
//! so:
//!
//! * `workers == 1` → 1 shard, behaves identically to the single-queue
//!   design (no scan cost, no extra mutexes).
//! * `workers ∈ {2, 3}` → 2-4 shards.
//! * `workers ≥ 5` → 8 shards (capped).
//!
//! The power-of-two count lets routing use a bitmask instead of a
//! modulo division.
//!
//! ## Lock-ordering and the parked-handshake
//!
//! Producers acquire the target shard's mutex, push, release, *then*
//! check the `parked` atomic. If any worker may be parked, the
//! producer acquires the `park` mutex briefly to call `notify_one`.
//! Workers, when parking, acquire the `park` mutex first, increment
//! `parked` *before* a final re-scan of all shards, then call
//! `cv.wait` (which atomically releases the `park` mutex).
//!
//! That ordering closes the lost-wakeup race two ways:
//!
//! 1. **Producer push before worker arms**: producer's release on the
//!    shard mutex happens-before the worker's acquire of that shard
//!    in the re-scan. Worker finds the push in the re-scan and
//!    doesn't wait.
//! 2. **Producer push after worker arms**: the cross-variable
//!    happens-before edge runs through the shard mutex, not directly
//!    through the `parked` atomic. Specifically:
//!
//!    * worker `parked.fetch_add(1, Release)` is *sequenced-before*
//!      its re-scan acquire of `shard[idx]`;
//!    * worker's `shard[idx]` unlock *synchronises-with* the
//!      producer's later `shard[idx]` lock (mutex pair);
//!    * producer's `shard[idx]` unlock is *sequenced-before* its
//!      `parked.load(Acquire)`.
//!
//!    By transitivity the worker's `fetch_add` happens-before the
//!    producer's load, so the load is guaranteed to observe ≥1 and
//!    the producer takes the `park.lock` + `notify_one` path. The
//!    `Release`/`Acquire` ordering on `parked` itself only governs
//!    coherence of the counter's value; the cross-variable visibility
//!    that closes the race comes from the shard mutex.
//!
//!    The producer's `park.lock` then blocks until the worker's
//!    `cv.wait` atomically releases `park.lock` and parks, so the
//!    `notify_one` always lands on a worker that is actually waiting.
//!
//! Lock-hold pairs never overlap on the producer side (shard then
//! park, with a release in between), so there is no AB-BA deadlock
//! even though the worker briefly holds `park` while acquiring
//! shards in the re-scan.

use async_task::Runnable;
use parking_lot::{Condvar, Mutex};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::cpu;

/// Hard cap on the shard count. Picked empirically: 8 reduces
/// producer-side mutex contention to roughly tokio's blocking-pool
/// level on the benches that matter, while keeping the worst-case
/// scan a 64-byte fast loop. Bigger isn't free — every empty pop
/// walks all shards.
const MAX_SHARDS: usize = 8;

/// Shared work queue. `push` notifies one waiter; `pop_blocking`
/// scans then parks until a runnable arrives or shutdown is signalled.
pub(crate) struct Queue {
	shards: Box<[Shard]>,
	/// `num_shards - 1`. `num_shards` is always a power of two.
	mask: usize,
	/// Held briefly by producers to notify, and by workers across the
	/// arm + re-scan + wait sequence.
	park: Mutex<()>,
	notify: Condvar,
	/// Approximate count of workers currently parked or about to
	/// park. Read by producers to decide whether to acquire `park`
	/// for a `notify_one`.
	parked: AtomicUsize,
	/// Set on threadpool drop. Workers observing this with all shards
	/// empty exit their loop.
	shutdown: AtomicBool,
}

struct Shard {
	inner: Mutex<VecDeque<Runnable>>,
}

impl Queue {
	pub(crate) fn new(num_workers: usize) -> Self {
		// `next_power_of_two()` panics on overflow; that's fine as
		// long as `num_workers <= usize::MAX / 2 + 1`. The public
		// `MAX_THREADS = 512` constant in `lib.rs` clamps the input,
		// so the panic is unreachable today — but if `MAX_THREADS`
		// is ever raised, keep that invariant in mind.
		let num_shards = num_workers.next_power_of_two().clamp(1, MAX_SHARDS);
		let shards: Vec<Shard> = (0..num_shards)
			.map(|_| Shard {
				inner: Mutex::new(VecDeque::new()),
			})
			.collect();
		Self {
			shards: shards.into_boxed_slice(),
			mask: num_shards - 1,
			park: Mutex::new(()),
			notify: Condvar::new(),
			parked: AtomicUsize::new(0),
			shutdown: AtomicBool::new(false),
		}
	}

	/// Push a runnable. Routes to a shard via the producer's
	/// thread-local CPU cache, so a producer pinned to (or
	/// long-resident on) core N always lands on shard `N & mask`.
	pub(crate) fn push(&self, runnable: Runnable) {
		let idx = cpu::current_cpu() & self.mask;
		self.shards[idx].inner.lock().push_back(runnable);
		// Fast path: if no worker may be parked, skip the park mutex
		// entirely. The Acquire ordering pairs with the worker's
		// Release on `parked.fetch_add`, so any worker that armed
		// before this load is observed.
		if self.parked.load(Ordering::Acquire) > 0 {
			// Acquire the park mutex briefly so the notify is
			// guaranteed to land on a worker that has either already
			// entered `cv.wait` (worker has released `park` atomically
			// with parking) or hasn't yet armed (in which case the
			// worker's re-scan will pick up our push before parking).
			let _g = self.park.lock();
			self.notify.notify_one();
		}
	}

	/// Pop the next runnable for the worker at `worker_idx`. The
	/// worker prefers shard `worker_idx & mask`, falling back to
	/// scanning remaining shards in cyclic order. Parks when all
	/// shards are empty; returns `None` only when the queue is empty
	/// AND shutdown has been signalled.
	pub(crate) fn pop_blocking(&self, worker_idx: usize) -> Option<Runnable> {
		let n = self.mask + 1;
		let my_shard = worker_idx & self.mask;

		loop {
			// Phase 1: lock-free scan from own shard forward. No
			// `park` lock held, so producers can push concurrently
			// to any shard without blocking on us.
			for offset in 0..n {
				let idx = (my_shard + offset) & self.mask;
				if let Some(r) = self.shards[idx].inner.lock().pop_front() {
					return Some(r);
				}
			}

			// Phase 2: arm parking. Acquire `park`, then bump
			// `parked` BEFORE the re-scan so any concurrent producer
			// load of `parked` sees us as armed and will notify if
			// we end up waiting.
			let mut park = self.park.lock();
			self.parked.fetch_add(1, Ordering::Release);

			// Re-scan all shards under the arm. Any push whose shard
			// release happens-before our acquire here is observed
			// and consumed without parking. Any push whose shard
			// release happens-after our acquire is one whose
			// producer will subsequently see `parked > 0` and try
			// to acquire `park` — that acquire blocks until our
			// `cv.wait` atomically releases `park`, so the
			// `notify_one` lands on us.
			let mut found = None;
			for offset in 0..n {
				let idx = (my_shard + offset) & self.mask;
				if let Some(r) = self.shards[idx].inner.lock().pop_front() {
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

			self.notify.wait(&mut park);
			self.parked.fetch_sub(1, Ordering::Release);
			// `park` dropped here. Retry from Phase 1.
		}
	}

	/// Signal shutdown and wake every worker. Workers see the
	/// shutdown flag and exit once their re-scan finds all shards
	/// empty.
	pub(crate) fn shutdown(&self) {
		self.shutdown.store(true, Ordering::Release);
		// Acquire `park` briefly so the broadcast can't lose a wakeup
		// to a worker mid-arm.
		let _g = self.park.lock();
		self.notify.notify_all();
	}
}
