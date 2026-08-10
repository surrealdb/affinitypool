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
//!   consistently lands on the same shard. Number of
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
//! CPU back to the producer instead of spinning against it.
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
//! counter. If any worker may be parked, the producer calls
//! [`Queue::wake_one`] to hand the wake to one specific worker.
//! Workers, when parking, acquire *their own* parker's mutex, bump
//! `parked` *before* a final re-scan of all shards and stealers, then
//! set `blocked` and `cv.wait` (which atomically releases that mutex).
//!
//! Two properties of that arrangement are load-bearing, and both are
//! easy to break by "optimising" them:
//!
//! 1. **The gate is a counter, not a set of per-worker bits.**
//!    `parked > 0` holds for the whole of every worker's armed
//!    interval, because only its owner decrements it. That is what
//!    licenses the step "producer read 0, therefore the arming
//!    worker's increment is later in the modification order" in the
//!    proof below. A bitmask whose bits other threads may clear has no
//!    such monotonicity: a third worker's clear can sit between an
//!    arming worker's set and the producer's load, so the producer can
//!    legitimately read "nobody parked" while a worker is on its way
//!    into `cv.wait`. That failure needs no weak-memory exotica — it
//!    happens under sequential consistency — so no amount of
//!    strengthening the orderings repairs it.
//! 2. **A wake must be *claimed*, not merely sent.** `notify_one` on a
//!    condvar with no waiter is silently dropped, so a producer that
//!    notifies a worker which has already left `cv.wait` has woken
//!    nobody. If it treats that as the wake it owed, a runnable can sit
//!    in the queue while other workers sleep — for as long as the
//!    unrelated task the "woken" worker went off to run. Hence
//!    `wake_one` tests `blocked` under the target's own mutex and only
//!    counts a candidate whose flag was still set.
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

/// Hard cap on the shard count. Picked empirically: 8 saturates
/// producer-side distribution on common topologies (≤8-core boxes
/// map one shard per core, 32-core boxes share 4 cores per shard)
/// while keeping the worst-case empty scan at 8 cheap CAS attempts.
const MAX_SHARDS: usize = 8;

/// Consecutive pushes to the same preferred shard before producer-
/// side spill kicks in. Multi-producer workloads rarely reach it
/// (their pushes interleave, resetting the counter), while a
/// single-producer fan-out trips it quickly so the work spreads
/// across shards before the other workers give up and park.
const SPILL_THRESHOLD: u32 = 8;

/// Re-scans performed with `spin_loop` backoff before a worker gives
/// up its CPU. Kept small: the point is to catch a runnable already
/// on its way, not to poll. Pure spinning is the cheapest way to win
/// a submit-and-await round trip, but it burns a core, so this phase
/// stays sub-microsecond.
const SPIN_ROUNDS: u32 = 3;

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
	/// Owning worker's index, so a self-spawn can bias its peer wake
	/// toward a neighbour rather than always toward worker 0.
	idx: usize,
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
	/// One parking slot per worker, indexed by worker index. A worker
	/// only ever blocks on its own slot, so producers waking different
	/// workers never contend, and a producer can pick *which* worker to
	/// wake instead of handing an arbitrary waiter to a shared condvar.
	/// See [`WorkerParker`] and [`Queue::wake_one`].
	parkers: Box<[CachePadded<WorkerParker>]>,
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

/// One worker's parking slot: the condvar it sleeps on plus the flag
/// that says whether it is actually asleep on it.
///
/// `blocked` lives *inside* the mutex rather than beside it as an
/// atomic, and that is load-bearing. A waker must be able to tell
/// "this worker is in `cv.wait` right now" from "this worker looked
/// parked a moment ago but has since woken", because a `notify_one`
/// delivered to a condvar with no waiter is silently dropped. If the
/// waker counted such a no-op as the wake it owed, a runnable could
/// sit in the queue while other workers slept. Reading and clearing
/// the flag under the same mutex the worker holds across its
/// arm/re-scan/wait sequence makes the check exact: see
/// [`Queue::wake_one`].
struct WorkerParker {
	/// `true` only while the owning worker is inside `cv.wait`. Set by
	/// the worker before waiting; cleared by whoever wakes it (or by
	/// the worker itself on a spurious wake).
	blocked: Mutex<bool>,
	cv: Condvar,
}

impl WorkerParker {
	fn new() -> Self {
		Self {
			blocked: Mutex::new(false),
			cv: Condvar::new(),
		}
	}
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
	/// replaces the default shard count (see [`MAX_SHARDS`]); it is
	/// rounded up to a power of two — the routing is a bitmask — and
	/// clamped to at least one and at most one shard per worker,
	/// since shards beyond the worker count only add empty-scan cost.
	pub(crate) fn new(num_workers: usize, shard_override: Option<usize>) -> Self {
		// `MAX_THREADS = 512` clamps the caller side, so
		// `next_power_of_two` cannot overflow here.
		let num_shards = match shard_override {
			Some(n) => n.max(1).next_power_of_two().clamp(1, num_workers.next_power_of_two()),
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
			parkers: (0..num_workers)
				.map(|_| CachePadded::new(WorkerParker::new()))
				.collect::<Vec<_>>()
				.into_boxed_slice(),
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
		// A respawned worker inherits slot `idx`, including its parker.
		// A worker only ever panics while running a job — never inside
		// `cv.wait` — so `blocked` should already be false; reset it
		// anyway so a stale `true` from any unforeseen path cannot make
		// a waker "claim" a worker that does not exist and count that
		// as the wake it owed.
		*self.parkers[idx].blocked.lock() = false;
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
				idx: ctx.idx,
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
				// Bias toward our neighbour: it is the peer most
				// likely to reach our deque first when it scans.
				self.wake_one(handle.idx.wrapping_add(1));
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
		let target = if self.mask == 0 {
			0
		} else {
			let preferred = cpu::current_shard_hint() & self.mask;
			SPILL.with(|s| {
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
			})
		};
		self.injectors[target].push(runnable);
		// SeqCst fence pairs with the worker's SeqCst fence
		// between `parked.fetch_add` and its re-scan; the pair
		// forms the Dekker invariant that prevents a lost wakeup
		// even when the queue itself is lock-free. See the
		// module-level proof.
		fence(Ordering::SeqCst);
		// Fast path: if no worker may be parked, touch no locks at
		// all. Otherwise hand the wake to the worker whose preferred
		// shard is the one we just pushed to, so its very first steal
		// attempt hits our injector.
		if self.parked.load(Ordering::Acquire) > 0 {
			self.wake_one(target);
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
			let backoff = Backoff::new();
			let mut spun = None;
			for round in 0..SPIN_ROUNDS + YIELD_ROUNDS {
				if round < SPIN_ROUNDS {
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

			// Phase 2: arm parking. Take *our own* parker's lock,
			// then bump the shared `parked` counter BEFORE the
			// re-scan so any concurrent producer load of `parked`
			// sees us as armed and will go looking for someone to
			// wake if we end up waiting.
			//
			// The gate stays a counter rather than, say, a bitmask
			// of parked workers: `parked > 0` holds for the whole of
			// every worker's armed interval, which is exactly what
			// lets "producer read 0" imply "the arming worker's
			// increment comes later in the modification order" in the
			// proof below. A per-worker bit that another worker may
			// clear has no such monotonicity, and the proof does not
			// transfer to it.
			let mut blocked = self.parkers[ctx.idx].blocked.lock();
			self.parked.fetch_add(1, Ordering::Release);
			// SeqCst fence pairs with the producer's SeqCst
			// fence between `injector.push` and `parked.load`.
			// The pair forms the Dekker invariant: if our
			// re-scan misses the push, the producer's load is
			// guaranteed to see `parked > 0` and take the
			// notify path. See module-level proof.
			fence(Ordering::SeqCst);

			// Re-scan under the arm. `Strict`: this read is the
			// worker's half of the Dekker pair, so it must be the
			// same injector access the proof is written against.
			if let Some(r) = self.scan(ctx, Probe::Strict) {
				self.parked.fetch_sub(1, Ordering::Release);
				return Some(r);
			}

			if self.shutdown.load(Ordering::Acquire) {
				self.parked.fetch_sub(1, Ordering::Release);
				return None;
			}

			// Publish "actually asleep" before waiting, under the
			// same lock a waker must take to observe it. A waker that
			// finds this `false` knows the notify would be dropped
			// and looks for another worker instead.
			*blocked = true;
			self.parkers[ctx.idx].cv.wait(&mut blocked);
			// Either a waker claimed us (it already set this false) or
			// this was a spurious wake; clear it either way so we are
			// never advertised as sleeping while awake.
			*blocked = false;
			self.parked.fetch_sub(1, Ordering::Release);
			// Our parker lock is dropped here. Retry from Phase 1.
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

	/// Wake one sleeping worker, preferring the one whose preferred
	/// shard is `hint` so the woken worker's first steal is local.
	/// Returns whether a worker was actually woken.
	///
	/// Walks candidates from `hint` and *claims* each one under its own
	/// mutex: a candidate counts only if `blocked` was still `true`
	/// when we held the lock. A candidate that has already left
	/// `cv.wait` is skipped rather than counted, because notifying its
	/// condvar would be a no-op — treating that as the wake we owed
	/// would leave a runnable queued while other workers slept.
	///
	/// The `lock()` is deliberately blocking, not `try_lock`. A worker
	/// holds its mutex across arm + re-scan + `cv.wait`, so blocking
	/// means we resolve against a settled state: either the worker has
	/// reached `cv.wait` (we claim and notify it) or it left the armed
	/// region (it found work, and will re-scan again later). A
	/// `try_lock` that failed would leave us unable to distinguish
	/// "about to sleep" from "already gone", which is exactly the case
	/// a lost wakeup hides in.
	///
	/// Returning `false` after a full walk is safe: every candidate was
	/// outside `cv.wait` while we held its mutex, so no worker is
	/// asleep, so there is nobody to lose a wakeup.
	fn wake_one(&self, hint: usize) -> bool {
		let n = self.parkers.len();
		let start = hint % n;
		// Pass 1, non-blocking: claim a worker that is simply asleep,
		// without ever waiting behind one that is mid-scan. This is the
		// common case and keeps the producer off the critical path.
		let mut contended = false;
		for k in 0..n {
			let parker = &self.parkers[(start + k) % n];
			match parker.blocked.try_lock() {
				Some(mut blocked) => {
					if *blocked {
						// Claim it: clear before notifying so a
						// concurrent producer walking the same
						// candidates skips this worker and goes on to
						// wake a different one.
						*blocked = false;
						parker.cv.notify_one();
						return true;
					}
				}
				None => contended = true,
			}
		}
		if !contended {
			// Every worker was awake and none was contended, so there
			// is nobody asleep to lose a wakeup.
			return false;
		}
		// Pass 2, blocking, over the slots we could not inspect. A
		// worker holding its own lock is inside arm + re-scan + wait,
		// and its re-scan may legitimately miss our push — that is
		// precisely the case the `parked` gate just told us to wake
		// someone for, so we cannot treat "busy" as "it will find it".
		// Blocking resolves against a settled state: either it reaches
		// `cv.wait` and we claim it, or it leaves the armed region
		// having found work.
		for k in 0..n {
			let parker = &self.parkers[(start + k) % n];
			let mut blocked = parker.blocked.lock();
			if *blocked {
				*blocked = false;
				parker.cv.notify_one();
				return true;
			}
		}
		false
	}

	/// Signal shutdown and wake every worker. Workers see the
	/// shutdown flag and exit once their re-scan finds everything
	/// empty.
	pub(crate) fn shutdown(&self) {
		self.shutdown.store(true, Ordering::Release);
		// Every worker sleeps on its own condvar, so shutdown has to
		// visit all of them rather than issue one broadcast. Taking
		// each worker's lock is what stops the wakeup being lost to a
		// worker that is mid-arm: the lock is only free once that
		// worker has either reached `cv.wait` (so the notify lands) or
		// left the armed region (so it will re-check `shutdown` on its
		// next pass and exit).
		//
		// Unlike `wake_one` this does not stop at the first claim and
		// ignores whether the claim succeeded — every worker must be
		// woken, not just one.
		for parker in &*self.parkers {
			let mut blocked = parker.blocked.lock();
			*blocked = false;
			parker.cv.notify_one();
		}
	}
}
