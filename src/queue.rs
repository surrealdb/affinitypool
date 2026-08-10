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
//!   `Injector<Runnable>`s. Producers pick a shard via a cached hash
//!   of their thread ID (see [`crate::cpu`]) — a given producer
//!   consistently lands on the same shard. The count defaults to
//!   `num_workers.next_power_of_two().min(MAX_SHARDS)` and can be
//!   overridden per pool by [`crate::Builder::shards`]; it is always a
//!   power of two so the routing is a bitmask, and a single-worker pool
//!   degenerates to one shard with no scan cost.
//! * **Per-worker deques.** Each worker owns one
//!   `crossbeam_deque::Worker<Runnable>`. The owner thread is the
//!   only producer to its own deque; pop/push from the owner is
//!   lock-free and uncontended. The fast path is: worker steals a
//!   *batch* from its preferred injector into its own deque, then
//!   drains the local deque without crossing any shared state until
//!   it empties.
//! * **Stealers.** A `Stealer<Runnable>` for each worker's deque is
//!   stored centrally so workers can steal from each other as a
//!   last resort before parking. Held in `ArcSwapOption<Stealer>`
//!   slots: the cross-worker scan path reads lock-free with a
//!   single atomic load, and the panic-respawn path atomically
//!   replaces a worker's slot when [`Sentry`] starts a fresh
//!   thread.
//!
//! Pop order: own deque → preferred injector → other injectors →
//! other workers' stealers → bounded spin → park.
//!
//! The two cross-worker steps visit every victim exactly once but
//! start at a per-worker random rotation, so idle workers don't
//! convoy onto the same injector in lockstep. On the unarmed scans
//! each victim is `is_empty()`-probed before the steal is attempted,
//! turning an idle pool's repeated scanning into shared loads rather
//! than a storm of contended CAS. A `Steal::Retry` moves on to the
//! next victim instead of spinning on the contended one.
//!
//! Before parking, a worker re-scans a few times with `spin_loop`
//! backoff ([`SPIN_ROUNDS`]) so a runnable already on its way is
//! caught without a futex round-trip, then a few more with
//! `yield_now` ([`YIELD_ROUNDS`]) so an oversubscribed pool hands the
//! CPU back to the producer instead of spinning against it. It is
//! skipped entirely below [`MIN_WORKERS_FOR_PREPARK`] workers, where
//! there is no contention to amortise and delaying the park is pure
//! latency.
//!
//! How much this phase contributes relative to the scan-path changes
//! above is **not yet established**: the only measurements available
//! were taken on a machine with heavy background load, which moves
//! these particular workloads by several-fold between runs of identical
//! code. Do not re-tune the constants below against numbers from a busy
//! machine.
//!
//! ## Producer spill
//!
//! Per-producer shard routing is a win when multiple producers are
//! active (the `multi_producer` benches): each producer hashes to its
//! own shard, so their traffic stays isolated. For a single-producer
//! `current_thread` runtime, every push hashes to the *same* shard and
//! the other N-1 workers idle-scan. To defeat that, producers track a
//! thread-local `(last_shard, count)`: after [`SPILL_THRESHOLD`]
//! consecutive pushes to the same preferred shard, subsequent pushes
//! rotate to neighbouring shards. Multi-producer workloads rarely trip
//! the threshold (their pushes interleave across distinct shards) and
//! stay fully affine.
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
use crossbeam_utils::{Backoff, CachePadded};
use parking_lot::{Condvar, Mutex};
use std::cell::Cell;
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering, fence};

use crate::cpu;

/// Per-worker stealer slot. `ArcSwapOption` so the steal-scan path
/// can read with a single atomic load (no mutex acquire), while
/// the panic-respawn path can still atomically replace a worker's
/// slot when a fresh thread takes over. `CachePadded` to keep slots
/// on separate cache lines and avoid false sharing during scans.
type StealerSlot = CachePadded<ArcSwapOption<Stealer<Runnable>>>;

/// Default cap on the shard count, overridable per pool via
/// [`crate::Builder::shards`]. Picked empirically: 8 saturates
/// producer-side distribution on common topologies (≤8-core boxes
/// map one shard per core, 32-core boxes share 4 cores per shard)
/// while keeping the worst-case empty scan at 8 cheap loads.
const MAX_SHARDS: usize = 8;

/// Consecutive pushes to the same preferred shard before producer-
/// side spill kicks in. Multi-producer workloads rarely reach it
/// (their pushes interleave, resetting the counter), while a
/// single-producer fan-out trips it quickly so the work spreads
/// across shards before the other workers give up and park.
const SPILL_THRESHOLD: u32 = 8;

/// Pools with fewer than this many workers skip the pre-park backoff
/// entirely and park as soon as a scan comes up empty.
///
/// The backoff earns its cost by avoiding futex round-trips and by
/// handing the CPU back to a producer that other workers are competing
/// with — both of which scale with the number of workers. On a one- or
/// two-worker pool there is almost no such contention to amortise, so
/// the delay before parking is close to pure added latency, which is
/// why those pools opt out.
const MIN_WORKERS_FOR_PREPARK: usize = 3;

/// Re-scans performed with `spin_loop` backoff before a worker gives
/// up its CPU. The point is to catch a runnable already on its way, not
/// to poll — spinning is the cheapest way to win a submit-and-await
/// round trip, but it burns a core.
///
/// Six matches `crossbeam_utils::Backoff`'s own spin/yield boundary
/// (`SPIN_LIMIT`), i.e. exactly the point at which that crate stops
/// spinning and starts yielding, so the two halves of this phase line up
/// with the backoff primitive driving them. That is a principled anchor
/// rather than a measured optimum — see the caveat in the module docs
/// about tuning these against a busy machine.
const SPIN_ROUNDS: u32 = 6;

/// Re-scans performed with `yield_now` between them, after
/// [`SPIN_ROUNDS`] and before parking. These exist for the
/// *oversubscribed* case: when workers outnumber free cores, an idle
/// worker that only spins starves the very producer it is waiting
/// for, so handing the CPU back beats both spinning and an immediate
/// park. Kept small too — each yield is a syscall.
const YIELD_ROUNDS: u32 = 4;

thread_local! {
	/// `(last_preferred_shard, consecutive_count)`. Reset when the
	/// preferred shard changes (a different producer thread is
	/// interleaved). When `count > SPILL_THRESHOLD`, the route rotates
	/// by `count - SPILL_THRESHOLD` shards.
	static SPILL: Cell<(usize, u32)> = const { Cell::new((usize::MAX, 0)) };

	/// When this thread is a worker for some `Queue`, holds raw
	/// pointers to that queue and the worker's local deque. Set
	/// by [`Queue::enter_worker_scope`] and cleared when the
	/// returned [`WorkerScope`] is dropped. [`Queue::push`] reads
	/// this to fast-path through the worker's own deque when the
	/// producer is itself a worker for this queue.
	static CURRENT_WORKER: Cell<Option<WorkerHandle>> = const { Cell::new(None) };
}

/// Pointer pair stashed in [`CURRENT_WORKER`] for the duration of
/// a worker thread's scope. Both pointers are valid only while
/// the corresponding [`WorkerScope`] is alive.
#[derive(Clone, Copy)]
struct WorkerHandle {
	queue: NonNull<Queue>,
	deque: NonNull<Worker<Runnable>>,
}

// `WorkerHandle` holds raw pointers and is only ever read on the
// thread that wrote it (the worker thread itself), so `Send` /
// `Sync` aren't needed and aren't requested. The thread-local
// machinery handles per-thread isolation.

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
	/// Test-only counter, incremented on every push that takes
	/// the foreign-producer path (i.e. did *not* engage the
	/// worker self-spawn fast path). Used by integration tests
	/// to assert the fast path engaged on a given workload.
	#[cfg(test)]
	pub(crate) foreign_pushes: AtomicUsize,
}

/// Per-worker state owned exclusively by one worker OS thread. The
/// `Worker<Runnable>` deque is `!Sync`; constructed inside the
/// worker closure via [`Queue::register_worker`] so panic-respawn
/// gets a fresh deque on each start.
pub(crate) struct WorkerContext {
	idx: usize,
	deque: Worker<Runnable>,
	/// xorshift32 state for randomising the *start offset* of the
	/// cross-shard and peer-steal scans. Without it every idle worker
	/// walks victims in the same order and they convoy onto the same
	/// injector, turning an idle pool into a CAS storm on one cache
	/// line. Seeded from `idx` (never zero — xorshift is absorbing at
	/// zero) so a respawned worker re-derives the same stream, which
	/// keeps runs reproducible. `Cell` is sound here because
	/// `WorkerContext` is owned by, and only ever touched from, its
	/// own worker thread — same justification as the `!Sync` deque.
	rng: Cell<u32>,
}

/// How hard a [`Queue::scan`] pass should work to notice a runnable.
/// See [`Queue::scan`] for why the armed re-scan must stay strict.
#[derive(Clone, Copy)]
enum Probe {
	/// Skip a victim whose `is_empty()` says empty.
	Cheap,
	/// Always attempt the steal.
	Strict,
}

/// Advance an xorshift32 and return the new state. Cheap enough
/// (3 shifts, 3 xors) to run on every scan pass.
#[inline]
fn next_rand(rng: &Cell<u32>) -> u32 {
	let mut x = rng.get();
	x ^= x << 13;
	x ^= x >> 17;
	x ^= x << 5;
	rng.set(x);
	x
}

/// RAII guard returned by [`Queue::enter_worker_scope`]. While
/// alive, the calling thread's [`CURRENT_WORKER`] holds a handle
/// to the queue + the worker's local deque so [`Queue::push`]
/// from the same thread can fast-path. On drop, clears the
/// thread-local so any later `push` from this thread falls back
/// to the foreign-producer path.
pub(crate) struct WorkerScope<'a> {
	_phantom: PhantomData<&'a ()>,
}

impl Drop for WorkerScope<'_> {
	fn drop(&mut self) {
		CURRENT_WORKER.with(|w| w.set(None));
	}
}

impl Queue {
	/// Build a queue for `num_workers` workers. `shard_override`
	/// replaces the default shard count (see [`MAX_SHARDS`]): it is
	/// clamped to `1..=num_workers` — shards beyond the worker count
	/// only add empty-scan cost — and then rounded up to a power of
	/// two, because the routing is a bitmask.
	pub(crate) fn new(num_workers: usize, shard_override: Option<usize>) -> Self {
		// Clamp *before* `next_power_of_two`, not after: the override is
		// arbitrary caller input, and `next_power_of_two` overflows (and
		// panics on a debug build) for anything above `1 << 63`.
		// `num_workers` is already clamped to `MAX_THREADS` by the
		// caller, so rounding it up cannot overflow.
		let num_shards = match shard_override {
			Some(n) => n.clamp(1, num_workers).next_power_of_two(),
			None => num_workers.next_power_of_two().clamp(1, MAX_SHARDS),
		};
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
			#[cfg(test)]
			foreign_pushes: AtomicUsize::new(0),
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
			// Mix the index so adjacent workers get unrelated streams,
			// and force the low bit so the state is never zero.
			rng: Cell::new((idx as u32).wrapping_mul(0x9E37_79B9) | 1),
		}
	}

	/// Register the calling thread as the active worker for this
	/// queue with the given context. Subsequent [`Queue::push`]
	/// calls from this thread that target this queue will route
	/// directly into the context's local deque, skipping the
	/// shared injector and the cross-thread wake-up handshake.
	/// The returned [`WorkerScope`] clears the thread-local
	/// registration on drop.
	///
	/// Must be called from the worker thread *after*
	/// [`Self::register_worker`], with the context held on the
	/// same stack frame for the scope's lifetime. The
	/// `'a` borrow on both `&self` and `ctx` makes it impossible
	/// for the scope to outlive either.
	pub(crate) fn enter_worker_scope<'a>(&'a self, ctx: &'a WorkerContext) -> WorkerScope<'a> {
		// `ctx.deque` is held on the worker thread's stack until
		// after the scope is dropped, so its address is stable
		// for the scope's lifetime.
		CURRENT_WORKER.with(|w| {
			w.set(Some(WorkerHandle {
				queue: NonNull::from(self),
				deque: NonNull::from(&ctx.deque),
			}))
		});
		WorkerScope {
			_phantom: PhantomData,
		}
	}

	/// Push a runnable. Routes to a shard by a hash of the producer's
	/// thread ID, with spill after [`SPILL_THRESHOLD`] consecutive
	/// pushes to the same shard.
	#[inline]
	pub(crate) fn push(&self, runnable: Runnable) {
		// Self-spawn fast path: if the calling thread is itself
		// a worker for THIS queue (set via `enter_worker_scope`
		// during worker startup), push directly into that
		// worker's local deque, skipping the Injector and the
		// shard routing.
		//
		// The spawning worker is *usually* also the consumer — it
		// returns to `pop_blocking` and pops its own deque, which
		// needs no cross-thread synchronisation. But it is not
		// guaranteed to get there: a worker that polls a
		// `SpawnFuture` and then drops it runs
		// `SpawnFuture::drop` -> `block_on_cancel` ->
		// `thread::park()`, which blocks until that very runnable
		// has run or been dropped. The runnable is sitting in this
		// worker's own deque, so the worker can no longer reach it
		// and only a *peer steal* can make progress. If every peer
		// is parked and we skip the wake, nothing ever wakes them:
		// the pool deadlocks for as long as the blocked worker
		// waits, which is forever.
		//
		// So the local deque is published with the same fenced
		// handshake the foreign path uses. The Dekker argument is
		// unchanged in shape — it only needs the producer's queue
		// access and the worker's steal to touch the same object —
		// with `Worker::push` and `Stealer::steal_batch_and_pop`
		// on this deque taking the place of the injector pair.
		if let Some(handle) = CURRENT_WORKER.with(|w| w.get())
			&& std::ptr::eq(handle.queue.as_ptr().cast_const(), self as *const Queue)
		{
			// SAFETY: `handle.deque` was installed by this same
			// thread's `enter_worker_scope`. The scope keeps the
			// `WorkerContext` (and the `Worker<Runnable>` it
			// owns) alive on this thread's stack for the
			// duration of the registration; on scope drop the
			// thread-local is cleared *before* `WorkerContext`
			// is dropped. So any non-`None` handle observed here
			// points to a live `Worker<Runnable>` owned by this
			// same thread. `Worker` is `!Sync` but used only
			// from its owner thread, so the `&self` `push` call
			// is sound.
			unsafe {
				handle.deque.as_ref().push(runnable);
			}
			// Same fence + parked check as the foreign path below.
			// A `Relaxed` peek at `parked` would not do: without the
			// fence, x86-TSO alone permits this store-then-load pair
			// to be observed out of order, so we could read
			// `parked == 0` while a peer that is about to sleep has
			// already missed our push.
			//
			// A 1-worker pool has no peer to wake, so a worker that
			// blocks on its own self-spawned runnable there still
			// deadlocks; that is inherent, and documented on
			// `Threadpool::spawn_local`.
			fence(Ordering::SeqCst);
			if self.parked.load(Ordering::Acquire) > 0 {
				let _g = self.park.lock();
				self.notify.notify_one();
			}
			return;
		}

		// Foreign-producer path: route via the shared Injector
		// and wake one parked worker if any.
		#[cfg(test)]
		self.foreign_pushes.fetch_add(1, Ordering::Relaxed);
		// Single-shard fast path: skip the shard-hint lookup, SPILL
		// thread-local, and bitmask arithmetic — they're all
		// dead work when `mask == 0` (which corresponds to a
		// 1-worker pool). The fence + park-check below still
		// run; producer↔worker synchronisation is independent of
		// shard count.
		if self.mask == 0 {
			self.injectors[0].push(runnable);
		} else {
			let preferred = cpu::current_shard_hint() & self.mask;
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
			if let Some(r) = self.scan(ctx, Probe::Cheap) {
				return Some(r);
			}

			// Phase 1b: bounded spin before committing to a park.
			// Parking costs a futex round-trip on both sides, which
			// dominates a submit-and-await round trip when the next
			// runnable is only a moment away. A few cheap re-scans
			// first let a worker that is about to be handed work skip
			// the syscall entirely.
			//
			// Two sub-phases, because the right thing to do with an
			// idle worker depends on whether the machine has a spare
			// core for it:
			//
			// * [`SPIN_ROUNDS`] of `spin_loop` — cheapest way to catch
			//   an imminent runnable when a core is free.
			// * [`YIELD_ROUNDS`] of `yield_now` — when workers
			//   outnumber cores, a spinning worker starves the
			//   producer it is waiting for, so give the CPU back
			//   before parking.
			//
			// Both are bounded and a worker that still finds nothing
			// falls through to arm and park, so an idle pool still
			// goes to sleep. Scans here are `Cheap` because they
			// repeat.
			let (spin_rounds, yield_rounds) = if self.stealers.len() >= MIN_WORKERS_FOR_PREPARK {
				(SPIN_ROUNDS, YIELD_ROUNDS)
			} else {
				(0, 0)
			};
			let backoff = Backoff::new();
			let mut spun = None;
			for round in 0..spin_rounds + yield_rounds {
				if round < spin_rounds {
					backoff.spin();
				} else {
					std::thread::yield_now();
				}
				if let Some(r) = self.scan(ctx, Probe::Cheap) {
					spun = Some(r);
					break;
				}
			}
			if let Some(r) = spun {
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

			// Re-scan under the arm. `Strict` is REQUIRED here, not a
			// preference: this read is the worker's half of the Dekker
			// pair, so it must be the same injector access the proof is
			// written against. Switching it to `Probe::Cheap` would
			// substitute `is_empty`'s weaker read for that access and
			// invalidate the lost-wakeup argument in the module docs.
			if let Some(r) = self.scan(ctx, Probe::Strict) {
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
	///
	/// Victim order for the two cross-worker steps is rotated by a
	/// per-worker RNG so concurrent idle workers don't convoy onto
	/// the same injector. Every victim is still visited exactly once
	/// per pass, so a non-empty queue cannot be skipped.
	///
	/// `probe` trades a strong guarantee for cheapness:
	///
	/// * `Probe::Cheap` tests `is_empty()` before each steal. An
	///   empty victim then costs a shared load instead of a failed
	///   CAS, which is what keeps an idle multi-worker pool from
	///   melting a cache line. Used on the unarmed scans, where a
	///   missed just-pushed runnable is harmless — the worker either
	///   loops and scans again or proceeds to arm, and the armed
	///   re-scan is strict.
	/// * `Probe::Strict` always attempts the steal. Used for the
	///   armed re-scan, where the read of each injector is
	///   load-bearing for the lost-wakeup proof (see module docs):
	///   the proof needs the worker's access to the injector to be
	///   ordered against the producer's push, so we keep the same
	///   operation the original proof was written against rather
	///   than reason about `is_empty`'s weaker read.
	#[inline]
	fn scan(&self, ctx: &WorkerContext, probe: Probe) -> Option<Runnable> {
		// A `Steal::Retry` means someone else's CAS beat ours, not
		// that the victim is empty. Spinning on that victim just
		// burns the contended cache line, so a retry moves on to the
		// next victim and we re-walk only if the whole pass found
		// nothing while at least one victim was contended. `Retry`
		// implies concurrent activity, so this terminates.
		loop {
			// 1. Own deque — owner-only, lock-free, zero contention.
			if let Some(r) = ctx.deque.pop() {
				return Some(r);
			}

			let mut contended = false;
			let n = self.mask + 1;
			let my_shard = ctx.idx & self.mask;

			// 2. Preferred injector. `steal_batch_and_pop` migrates
			//    a batch into our deque and returns one runnable;
			//    subsequent pops in step 1 hit the local deque
			//    without any cross-shard traffic.
			match self.steal_injector(my_shard, ctx, probe) {
				Steal::Success(r) => return Some(r),
				Steal::Retry => contended = true,
				Steal::Empty => {}
			}

			// 3. Other injectors, every one visited once, starting at
			//    a random rotation.
			if n > 1 {
				let span = n - 1;
				let start = next_rand(&ctx.rng) as usize % span;
				for k in 0..span {
					let idx = (my_shard + 1 + (start + k) % span) & self.mask;
					match self.steal_injector(idx, ctx, probe) {
						Steal::Success(r) => return Some(r),
						Steal::Retry => contended = true,
						Steal::Empty => {}
					}
				}
			}

			// 4. Other workers' deques, last resort. Lock-free
			//    `ArcSwapOption::load` returns an `Arc<Stealer>` we
			//    can steal through without any per-slot mutex.
			let num_workers = self.stealers.len();
			if num_workers > 1 {
				let span = num_workers - 1;
				let start = next_rand(&ctx.rng) as usize % span;
				for k in 0..span {
					let victim = (ctx.idx + 1 + (start + k) % span) % num_workers;
					let Some(stealer) = self.stealers[victim].load_full() else {
						continue;
					};
					if matches!(probe, Probe::Cheap) && stealer.is_empty() {
						continue;
					}
					match stealer.steal_batch_and_pop(&ctx.deque) {
						Steal::Success(r) => return Some(r),
						Steal::Retry => contended = true,
						Steal::Empty => {}
					}
				}
			}

			if !contended {
				return None;
			}
		}
	}

	/// One steal attempt against injector `idx`, skipped entirely on
	/// an apparently-empty injector when `probe` allows it.
	#[inline]
	fn steal_injector(&self, idx: usize, ctx: &WorkerContext, probe: Probe) -> Steal<Runnable> {
		if matches!(probe, Probe::Cheap) && self.injectors[idx].is_empty() {
			return Steal::Empty;
		}
		self.injectors[idx].steal_batch_and_pop(&ctx.deque)
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
