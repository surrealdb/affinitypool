//! Sharded MPMC queue used by `Threadpool` to deliver `Runnable`s from
//! producers to worker threads. Backed by `crossbeam_deque` —
//! `Injector`s for cross-thread handoff and per-worker `Worker` deques
//! for the steady-state hot path.
//!
//! ## Architecture
//!
//! Three pools of storage:
//!
//! * **Sharded injectors.** Up to [`MAX_SHARDS`] lock-free MPMC
//!   `Injector<Runnable>`s. Producers pick a shard via the
//!   thread-local CPU cache (see [`crate::cpu`]) — a producer running
//!   on core *N* consistently lands on shard `N & mask`. Number of
//!   shards is `num_workers.next_power_of_two().min(MAX_SHARDS)` so
//!   the routing is a bitmask and a single-worker pool degenerates to
//!   one shard with no scan cost.
//! * **Per-worker deques.** Each worker owns one
//!   `crossbeam_deque::Worker<Runnable>`. The owner thread is the
//!   only producer to its own deque; pop/push from the owner is
//!   lock-free and uncontended. The fast path is: worker steals a
//!   *batch* from its preferred injector into its own deque, then
//!   drains the local deque without crossing any shared state until
//!   it empties.
//! * **Stealers.** A `Stealer<Runnable>` for each worker's deque is
//!   stored centrally so workers can steal from each other as a
//!   last resort before parking. Wrapped in
//!   `Mutex<Option<Stealer>>` so the panic-respawn path can replace
//!   a worker's slot when [`Sentry`] starts a fresh thread — the
//!   mutex is uncontended outside that rare path.
//!
//! Pop order: own deque → preferred injector → other injectors,
//! cyclic → other workers' stealers → park.
//!
//! ## Producer spill
//!
//! Pure CPU-affinity routing is a win when multiple producers
//! naturally span cores (the `multi_producer` benches). For a
//! single-producer `current_thread` runtime, every push pins to one
//! shard and the other N-1 workers idle-scan. To defeat that,
//! producers track a thread-local `(last_shard, count)`: after
//! [`SPILL_THRESHOLD`] consecutive pushes to the same preferred
//! shard, subsequent pushes rotate to neighbouring shards. Multi-
//! producer workloads never trip the threshold (their preferred
//! shard oscillates as the OS schedules them) and stay fully
//! affine.
//!
//! ## Lock-ordering and the parked-handshake
//!
//! Producers push to a shard's injector, then check the `parked`
//! atomic. If any worker may be parked, the producer briefly takes
//! the `park` mutex to call `notify_one`. Workers, when parking,
//! acquire `park` first, bump `parked` *before* a final re-scan of
//! all shards and stealers, then `cv.wait` (which atomically
//! releases `park`).
//!
//! Unlike the previous mutex-shard design, the cross-thread
//! happens-before edge no longer flows through a shard mutex.
//! [`crossbeam_deque::Injector`] is lock-free; pushes and steals
//! synchronise through the injector's internal atomics, but those
//! orderings alone aren't enough to close the producer↔worker
//! race on `parked`. The queue therefore inserts a
//! [`fence(SeqCst)`] between each side's queue access and its
//! `parked` access — the textbook Dekker pattern.
//!
//! [`fence(SeqCst)`]: std::sync::atomic::fence
//!
//! **Proof sketch (Dekker fence pattern).** With the fences in
//! place, the producer's `Injector::push` is sequenced-before its
//! `fence(SeqCst)`, which is sequenced-before its
//! `parked.load`; the worker's `parked.fetch_add` is
//! sequenced-before its `fence(SeqCst)`, which is sequenced-before
//! its `Injector::steal`. Both `fence(SeqCst)`s appear in a single
//! SeqCst total order.
//!
//! Assume for contradiction that a wakeup is lost — i.e., the
//! producer's `parked.load` reads 0 (so producer takes the fast
//! path and skips `notify_one`) AND the worker's `Injector::steal`
//! finds nothing (so the worker proceeds into `cv.wait`).
//!
//! * `parked.load = 0` means the worker's `parked.fetch_add` is
//!   *after* the producer's `parked.load` in `parked`'s
//!   modification order. By the SeqCst fence rule, the worker's
//!   fence is then after the producer's fence in SeqCst order.
//! * `Injector::steal = empty` means the producer's `Injector::push`
//!   is *after* the worker's `Injector::steal` in the injector's
//!   modification order. By the SeqCst fence rule, the producer's
//!   fence is then after the worker's fence in SeqCst order.
//!
//! These two conclusions contradict — the fences can't both be
//! before each other in the SeqCst total order. So at least one
//! of (`parked.load = 0`) or (`Injector::steal = empty`) is false,
//! and the wakeup is delivered.
//!
//! Either way: if the worker's re-scan finds the runnable, the
//! worker doesn't park. If the producer's notify path runs, it
//! synchronises through `park.lock()` — which blocks until the
//! worker is already in `cv.wait` (since the worker holds `park`
//! across arm + re-scan and `cv.wait` atomically releases it).
//!
//! [`Sentry`]: crate::sentry::Sentry

use arc_swap::ArcSwapOption;
use async_task::Runnable;
use crossbeam_deque::{Injector, Steal, Stealer, Worker};
use crossbeam_utils::CachePadded;
use parking_lot::{Condvar, Mutex};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering, fence};

use crate::cpu;

/// Per-worker stealer slot. `ArcSwapOption` so the steal-scan path
/// can read with a single atomic load (no mutex acquire), while
/// the panic-respawn path can still atomically replace a worker's
/// slot when a fresh thread takes over. `CachePadded` to keep slots
/// on separate cache lines and avoid false sharing during scans.
type StealerSlot = CachePadded<ArcSwapOption<Stealer<Runnable>>>;

/// Hard cap on the shard count. Picked empirically: 8 saturates
/// producer-side distribution on common topologies (≤8-core boxes
/// map one shard per core, 32-core boxes share 4 cores per shard)
/// while keeping the worst-case empty scan at 8 cheap CAS attempts.
const MAX_SHARDS: usize = 8;

/// Consecutive pushes to the same preferred shard before producer-
/// side spill kicks in. Picked empirically: large enough that
/// genuinely-affine producers (multi-producer benches) never trip
/// it, small enough that a single-producer fan-out distributes
/// across workers before they have time to park.
const SPILL_THRESHOLD: u32 = 32;

thread_local! {
	/// `(last_preferred_shard, consecutive_count)`. Reset when the
	/// preferred shard changes (producer moved CPUs or another
	/// producer is interleaved). When `count > SPILL_THRESHOLD`, the
	/// route rotates by `count - SPILL_THRESHOLD` shards.
	static SPILL: Cell<(usize, u32)> = const { Cell::new((usize::MAX, 0)) };
}

/// Shared work queue. `push` notifies one waiter; `pop_blocking`
/// drains local + steals from shards + steals from peers, then
/// parks until a runnable arrives or shutdown is signalled.
pub(crate) struct Queue {
	/// Lock-free sharded injectors. Producers push here.
	injectors: Box<[CachePadded<Injector<Runnable>>]>,
	/// Stealers for every worker's local deque, indexed by worker
	/// index. See [`StealerSlot`] for the cache-padding + Option
	/// rationale.
	stealers: Box<[StealerSlot]>,
	/// `num_shards - 1`. `num_shards` is always a power of two.
	mask: usize,
	/// Held briefly by producers to notify, and by workers across
	/// the arm + re-scan + wait sequence.
	park: Mutex<()>,
	notify: Condvar,
	/// Approximate count of workers currently parked or about to
	/// park. Read by producers (Acquire) and incremented by
	/// workers (Release); a SeqCst fence on each side between the
	/// queue access and the parked access closes the lost-wakeup
	/// race. See module docs for the Dekker-fence proof.
	parked: AtomicUsize,
	/// Set on threadpool drop. Workers observing this with every
	/// shard and stealer empty exit their loop.
	shutdown: AtomicBool,
}

/// Per-worker state owned exclusively by one worker OS thread. The
/// `Worker<Runnable>` deque is `!Sync`; constructed inside the
/// worker closure via [`Queue::register_worker`] so panic-respawn
/// gets a fresh deque on each start.
pub(crate) struct WorkerContext {
	idx: usize,
	deque: Worker<Runnable>,
}

impl Queue {
	pub(crate) fn new(num_workers: usize) -> Self {
		// `MAX_THREADS = 512` clamps the caller side, so
		// `next_power_of_two` cannot overflow here.
		let num_shards = num_workers.next_power_of_two().clamp(1, MAX_SHARDS);
		let injectors: Vec<CachePadded<Injector<Runnable>>> =
			(0..num_shards).map(|_| CachePadded::new(Injector::new())).collect();
		let stealers: Vec<StealerSlot> =
			(0..num_workers).map(|_| CachePadded::new(ArcSwapOption::empty())).collect();
		Self {
			injectors: injectors.into_boxed_slice(),
			stealers: stealers.into_boxed_slice(),
			mask: num_shards - 1,
			park: Mutex::new(()),
			notify: Condvar::new(),
			parked: AtomicUsize::new(0),
			shutdown: AtomicBool::new(false),
		}
	}

	/// Construct per-worker state and register the worker's
	/// `Stealer` in the queue's slot for `idx`. Called once from
	/// each worker thread's entry point, *and* on respawn — the
	/// new thread overwrites the slot with a fresh stealer. Any
	/// stealer reference held momentarily by another worker keeps
	/// the dropped buffer alive (the buffer is `Arc`-shared
	/// between `Worker` and its `Stealer`s) but observes only
	/// empty steals once the original `Worker` is gone.
	pub(crate) fn register_worker(&self, idx: usize) -> WorkerContext {
		let deque = Worker::new_fifo();
		let stealer = deque.stealer();
		self.stealers[idx].store(Some(Arc::new(stealer)));
		WorkerContext {
			idx,
			deque,
		}
	}

	/// Push a runnable. Routes to a shard by the producer's
	/// thread-local CPU cache, with spill after
	/// [`SPILL_THRESHOLD`] consecutive pushes to the same shard.
	#[inline]
	pub(crate) fn push(&self, runnable: Runnable) {
		// Single-shard fast path: skip the CPU lookup, SPILL
		// thread-local, and bitmask arithmetic — they're all
		// dead work when `mask == 0` (which corresponds to a
		// 1-worker pool). The fence + park-check below still
		// run; producer↔worker synchronisation is independent of
		// shard count.
		if self.mask == 0 {
			self.injectors[0].push(runnable);
		} else {
			let preferred = cpu::current_cpu() & self.mask;
			let target = SPILL.with(|s| {
				let (last, count) = s.get();
				let new_count = if last == preferred {
					count.saturating_add(1)
				} else {
					1
				};
				s.set((preferred, new_count));
				if new_count <= SPILL_THRESHOLD {
					preferred
				} else {
					// Rotate by (count - threshold) shards once
					// we've tripped. As `count` grows, subsequent
					// pushes cycle through all shards, draining
					// the otherwise-pinned single producer evenly.
					(preferred + (new_count - SPILL_THRESHOLD) as usize) & self.mask
				}
			});
			self.injectors[target].push(runnable);
		}
		// SeqCst fence pairs with the worker's SeqCst fence
		// between `parked.fetch_add` and its re-scan; the pair
		// forms the Dekker invariant that prevents a lost wakeup
		// even when the queue itself is lock-free. See the
		// module-level proof.
		fence(Ordering::SeqCst);
		// Fast path: if no worker may be parked, skip the park
		// mutex.
		if self.parked.load(Ordering::Acquire) > 0 {
			// Acquire `park` briefly so the notify is guaranteed
			// to land on a worker that has either already entered
			// `cv.wait` (worker has released `park` atomically
			// with parking) or hasn't yet armed (in which case
			// the worker's re-scan will pick up our push before
			// parking).
			let _g = self.park.lock();
			self.notify.notify_one();
		}
	}

	/// Pop the next runnable for the given worker context. The
	/// worker prefers its own deque, then injector
	/// `worker_idx & mask`, falling back to scanning remaining
	/// injectors in cyclic order, then stealing from other
	/// workers. Parks when nothing is found; returns `None` only
	/// when shutdown has been signalled and everything is empty.
	#[inline]
	pub(crate) fn pop_blocking(&self, ctx: &WorkerContext) -> Option<Runnable> {
		loop {
			// Phase 1: lock-free scan. No park lock held, so
			// producers can push concurrently without blocking
			// on us.
			if let Some(r) = self.scan(ctx) {
				return Some(r);
			}

			// Phase 2: arm parking. Acquire `park`, then bump
			// `parked` BEFORE the re-scan so any concurrent
			// producer load of `parked` sees us as armed and
			// will notify if we end up waiting.
			let mut park = self.park.lock();
			self.parked.fetch_add(1, Ordering::Release);
			// SeqCst fence pairs with the producer's SeqCst
			// fence between `injector.push` and `parked.load`.
			// The pair forms the Dekker invariant: if our
			// re-scan misses the push, the producer's load is
			// guaranteed to see `parked > 0` and take the
			// notify path. See module-level proof.
			fence(Ordering::SeqCst);

			// Re-scan under the arm.
			if let Some(r) = self.scan(ctx) {
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

	/// One lock-free scan pass: own deque → preferred injector →
	/// other injectors → other workers' stealers. Returns the
	/// first runnable found, or `None` if everything is empty.
	#[inline]
	fn scan(&self, ctx: &WorkerContext) -> Option<Runnable> {
		// 1. Own deque — owner-only, lock-free, zero contention.
		if let Some(r) = ctx.deque.pop() {
			return Some(r);
		}

		let n = self.mask + 1;
		let my_shard = ctx.idx & self.mask;

		// 2. Preferred injector. `steal_batch_and_pop` migrates
		//    a batch into our deque and returns one runnable;
		//    subsequent pops in step 1 hit the local deque
		//    without any cross-shard traffic.
		if let Some(r) = retry_steal(|| self.injectors[my_shard].steal_batch_and_pop(&ctx.deque)) {
			return Some(r);
		}

		// 3. Other injectors in cyclic order, starting from the
		//    shard adjacent to our preferred one.
		for offset in 1..n {
			let idx = (my_shard + offset) & self.mask;
			if let Some(r) = retry_steal(|| self.injectors[idx].steal_batch_and_pop(&ctx.deque)) {
				return Some(r);
			}
		}

		// 4. Other workers' deques, last resort. Lock-free
		//    `ArcSwapOption::load` returns an `Arc<Stealer>` we
		//    can steal through without any per-slot mutex.
		let num_workers = self.stealers.len();
		for offset in 1..num_workers {
			let victim = (ctx.idx + offset) % num_workers;
			if let Some(stealer) = self.stealers[victim].load_full()
				&& let Some(r) = retry_steal(|| stealer.steal_batch_and_pop(&ctx.deque))
			{
				return Some(r);
			}
		}

		None
	}

	/// Signal shutdown and wake every worker. Workers see the
	/// shutdown flag and exit once their re-scan finds everything
	/// empty.
	pub(crate) fn shutdown(&self) {
		self.shutdown.store(true, Ordering::Release);
		// Acquire `park` briefly so the broadcast can't lose a
		// wakeup to a worker mid-arm.
		let _g = self.park.lock();
		self.notify.notify_all();
	}
}

/// `crossbeam_deque::Steal` returns `Retry` on transient CAS
/// failure. Spin until `Success` or `Empty`.
#[inline]
fn retry_steal(mut f: impl FnMut() -> Steal<Runnable>) -> Option<Runnable> {
	loop {
		match f() {
			Steal::Success(r) => return Some(r),
			Steal::Empty => return None,
			Steal::Retry => continue,
		}
	}
}
