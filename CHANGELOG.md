# Changelog

## Unreleased

### Performance

- **Idle workers no longer thrash the queue.** An idle multi-worker
  pool used to burn itself on the steal path: every worker walked
  victims in the same order, hammered a contended
  `steal_batch_and_pop` CAS, and spun in place on `Steal::Retry`.
  Three changes — a per-worker random start offset for the
  cross-shard and peer-steal walks, an `is_empty()` probe before each
  steal on the unarmed scans (a shared load instead of a failed CAS),
  and moving on to the next victim on `Retry` instead of spinning on
  the contended one — turn that storm back into useful work. Adding
  workers no longer makes the pool slower.
- **Bounded spin before parking.** A worker now re-scans a few times
  with `spin_loop` backoff and then a few more with `yield_now`
  before it arms and parks, so a runnable already on its way is
  picked up without a futex round-trip. The `yield_now` rounds matter
  for oversubscribed pools, where a purely spinning worker starves
  the producer it is waiting for.
- **Faster producer spill.** `SPILL_THRESHOLD` drops from 32 to 8, so
  a single producer fanning out spreads across shards sooner instead
  of piling onto one while the other workers idle.

Measured on an idle AMD Ryzen Threadripper 9970X (32 cores / 64 threads,
one NUMA node), Ubuntu 24.04, kernel 6.8, full criterion sampling. All
three revisions were benchmarked back-to-back in one session on a machine
at load average 0.00, with rayon built from the same runs as an unchanged
control. Every figure below has non-overlapping criterion confidence
intervals (median CI width 11%).

**24 of 27 microbenchmarks faster, 3 slower.**

| workload | 0.7.0 + prior fixes | this release | |
|---|---|---|---|
| `steal_imbalance/8_workers` | 25.4 ms | 3.64 ms | 6.98x |
| `spawn_overhead/4_workers/1` | 6.14 us | 1.11 us | 5.51x |
| `steady_state_busy/8_workers` | 98.2 ms | 17.9 ms | 5.49x |
| `park_unpark_handshake/4_workers` | 5.83 us | 1.09 us | 5.32x |
| `spawn_overhead/1_worker/1` | 5.37 us | 1.17 us | 4.60x |
| `spawn_local_overhead/4_workers/1000` | 4.87 ms | 1.09 ms | 4.46x |
| `steady_state_busy/4_workers` | 18.2 ms | 4.47 ms | 4.07x |
| `steal_imbalance/2_workers` | 9.33 ms | 2.46 ms | 3.80x |
| `steal_imbalance/4_workers` | 10.7 ms | 3.25 ms | 3.30x |
| `multi_producer_contention/2p_4w` | 2.66 ms | 878 us | 3.02x |
| `per_core_steady_state` (64 workers) | 203 ms | 74.2 ms | 2.74x |
| `multi_producer_contention/4p_1w` | 802 us | 999 us | 0.80x |
| `multi_producer_contention/2p_1w` | 436 us | 511 us | 0.85x |
| `multi_producer_contention/8p_1w` | 1.88 ms | 2.22 ms | 0.85x |

The three regressions share a shape: one worker fed by several
producers, so the worker is saturated and the pre-park backoff is pure
delay before a park it was always going to take.

Against `rayon::ThreadPool::spawn` on the same machine and runs,
affinitypool is now ahead on 14 of 17 head-to-head workloads, including
`multi_producer/4p_4w` (475 us vs 1.08 ms) and `spawn_overhead/4w/10000`
(1.99 ms vs 3.46 ms). The remaining three are within noise or close:
`round_trip/1` 1.07 us vs 0.94 us, `round_trip/8` 2.00 us vs 1.91 us
(neither statistically separable), and `spawn_overhead/1w/100` 21.8 us vs
19.6 us. Earlier releases lost single-task latency to rayon by 4-6x on
this platform; the pre-park backoff closes that.

One caveat worth stating: these gains assume spare CPU. On a
CPU-contended host a spinning worker competes with the producer it is
waiting for, and an earlier revision of this work measured the opposite
sign on a loaded laptop. The backoff yields as well as spins to limit
that, but a heavily oversubscribed deployment should measure rather than
assume.

### Added

- **`Builder::shards(n)`** to override how many queue shards producers
  distribute work across (default: one per worker, capped at 8). More
  shards means less contention between concurrent producers but more
  empty queues for an idle worker to scan. Rounded up to a power of
  two and clamped to at most one shard per worker.

### Breaking changes

- **`spawn_local` is now `unsafe`.** Both `Threadpool::spawn_local` and
  the free `affinitypool::spawn_local` are marked `unsafe fn`. They
  accept closures that borrow non-`'static` data, and their soundness
  depends on the returned future's destructor running (it blocks until
  the worker stops touching the borrows). Because safe code can skip a
  destructor by leaking the future (`mem::forget`, `Box::leak`, an
  `Rc`/`Arc` cycle, a leaked enclosing future), that obligation cannot
  be guaranteed by the type system and is now the caller's
  responsibility via an `# Safety` contract: **do not leak the returned
  future while it borrows non-`'static` data.** This is the classic
  leak-based unsoundness (the pre-1.0 `std::thread::scoped` hole); there
  is no sound, fully safe `spawn_local` in async Rust — the only
  leak-proof design is a synchronous scoped API, which has no
  `.await`-able equivalent. Migration: wrap existing calls in `unsafe`;
  the common `pool.spawn_local(..).await` (or dropping the future
  normally) already upholds the contract. Closures that capture only
  `'static` data should move to the safe `spawn`. See `tests/unsound.rs`
  for a demonstration of the hazard.

### Removed

- **CPU-core affinity pinning has been removed entirely.** The
  `affinity` module and its `libc`/`winapi` platform FFI are gone,
  along with per-worker core pinning. Pinning was off by default, a
  no-op on Apple Silicon, useless-to-harmful under containers/VMs, and
  unproven on the workloads this pool targets; removing it drops all
  first-party platform `unsafe` and both platform dependencies.
  `Builder::thread_per_core(true)` still spawns one worker per core (a
  thread count) but no longer pins — placement is left to the OS
  scheduler. *(Breaking: `affinitypool::affinity` is no longer a public
  module.)*

### Changed

- **Shard routing no longer queries the CPU.** Producers now pick a
  shard from a cached hash of the thread ID instead of `sched_getcpu()`
  / `GetCurrentProcessorNumber()`. No syscall on the push path, no
  platform dependency, and it works under miri; per-producer shard
  stickiness is unchanged.

### Fixed

- **Deadlock when a worker blocked on its own self-spawned runnable.**
  The self-spawn fast path (a `pool.spawn(..)` from inside a worker
  closure, which pushes into that worker's own deque) issued no wake to
  parked peers, on the reasoning that the spawning worker is also the
  consumer. That reasoning does not hold: a worker that polls a
  `SpawnFuture` and then drops it runs `block_on_cancel` ->
  `thread::park()`, blocking until the runnable it just queued has
  stopped. That runnable is in the blocked worker's own deque, so only
  a peer steal can complete it — and with every peer parked and no
  wake issued, nothing ever woke them. The pool hung indefinitely.
  The self-spawn path now performs the same fenced wake handshake as a
  foreign push (`fence(SeqCst)` then the `parked` check — a `Relaxed`
  peek would be unsound, since x86-TSO alone lets the store-then-load
  pair be observed out of order). A one-worker pool has no peer to
  wake and still self-deadlocks on that pattern; that is inherent and
  is now documented on `Threadpool::spawn_local`.

### Behavioural notes

- **Worker self-spawn fast path.** When a closure running on a
  worker thread calls `pool.spawn(...)`, the new task is pushed
  directly into that worker's own local deque instead of routing
  through the shared injector, so work stays biased toward the
  spawning worker (good for locality) while remaining visible to peer
  stealers. Unlike earlier releases it now also wakes a parked peer;
  see the fix above for why that is required rather than merely
  desirable.

## 0.6.0 — 2026-05-24 — async-task rewrite + sharded queue

A major internal rewrite that closes the 8-15× performance gap versus
`tokio::task::spawn_blocking` and ends up beating it on most
heavy-contention workloads while preserving CPU affinity — the
feature this library exists for. Public trait methods
(`Threadpool::new`, `spawn`, `spawn_local`, `Builder`, `affinity::*`,
global `spawn`/`spawn_local`, `Error`, `MAX_THREADS`) are unchanged;
see Breaking changes for two behavioural shifts and one concrete-type
rename.

### Performance

Head-to-head with `tokio::task::spawn_blocking` (`--quick` criterion
run, system idle):

| Bench | 0.5.0 | this PR | Tokio | this PR vs Tokio |
|---|---|---|---|---|
| `spawn_overhead/4w/10000` | 48.4 ms | **6.4 ms** | 21.2 ms | **AP wins 3.3×** |
| `spawn_overhead/4w/100` | 728 µs | **66.7 µs** | 144 µs | **AP wins 2.2×** |
| `spawn_overhead/1w/10000` | 68.8 ms | 1.62 ms | 2.27 ms | **AP wins 1.4×** |
| `burst_drain/100k` | n/a | **50.7 ms** | 153 ms | **AP wins 3.0×** |
| `burst_drain/1M` | n/a | 564 ms | 683 ms | AP wins 1.2× |
| `concurrent_pipeline` (8p×100k, 8w) | n/a | **316 ms** | 463 ms | **AP wins 1.5×** |
| `multi_producer/8p_4w` | 6.68 ms | **2.15 ms** | 6.23 ms | **AP wins 2.9×** |
| `multi_producer/4p_4w` | 5.03 ms | 1.06 ms | 1.35 ms | **AP wins 1.3×** |
| `round_trip/4w` | 6.30 µs | 4.93 µs | 7.10 µs | **AP wins 1.4×** |
| `realistic_cost/100ns` | n/a | 4.57 ms | 21.0 ms | **AP wins 4.6×** |
| `realistic_cost/250µs` | n/a | 99.7 ms | 98.9 ms | parity (work dominates) |
| `sustained_throughput` | n/a | 52.3 ms | 61.8 ms | AP wins 1.2× |

Sustained throughput: **~1.9 M tasks/s on 4 workers, ~2.5 M tasks/s
on 8 workers** (`concurrent_pipeline`).

### Architecture

* **Task layout: `async-task`.** Single allocation per spawn via
  [`async-task`](https://crates.io/crates/async-task) v4. Replaces
  the hand-rolled `Job<F, R>` layout, `OwnedTask` vtable, and
  `AtomicWaker`. async-task is mature, used by smol/fuchsia, and
  loom-tested upstream.
* **Sharded queue.** `parking_lot::Mutex<VecDeque<Runnable>>` ×
  `num_shards`, where `num_shards = num_workers.next_power_of_two().min(8)`.
  `workers == 1` → 1 shard (no scan cost, no regression).
  `workers ≥ 5` → 8 shards (capped). Power-of-two count enables
  bitmask routing instead of modulo division.
* **CPU-affinity routing.** Producers pick a shard via a thread-local
  cache of `sched_getcpu()` (Linux) / `GetCurrentProcessorNumber()`
  (Windows), refreshed every 64 pushes. Other platforms hash the
  thread ID — less geographical, but stable per producer thread,
  which is what gets you cache-locality for long-lived producers.
* **Shard scanning, not work-stealing.** Each worker has a preferred
  shard (`worker_idx & mask`); on empty, scans remaining shards in
  cyclic order before parking. No private deques, no victim
  selection, no `STEAL_RETRY_BUDGET` spin loop, no SeqCst fence
  handshake.
* **Lost-wakeup-free park protocol.** Producers acquire the shard
  mutex, push, release, then check a `parked` atomic. If any worker
  may be parked, the producer briefly acquires the `park` mutex to
  call `notify_one`. Workers, when parking, hold the `park` mutex
  across `parked.fetch_add` and a final re-scan of all shards
  before `cv.wait` — so any push whose `parked.load` sees the
  worker armed must serialise through `park.lock`, and the worker's
  `cv.wait` atomically releases that lock with starting to wait.
  See `src/queue.rs` for the full proof sketch and
  `tests/loom_queue.rs` for the exhaustive model.

### Breaking changes

- `Threadpool::spawn` is now a synchronous function returning
  `impl Future<Output = R> + Send + 'static` rather than an
  `async fn`. The closure is scheduled **immediately** when `spawn` is
  called, not on the first poll of the returned future. Callers that
  used `pool.spawn(closure).await` are unaffected. Callers that built
  many futures and awaited them later will see the closures start
  running in parallel right away — typically a performance win and
  the behavior most users expect.

- Dropping the future returned by `Threadpool::spawn` before it
  resolves now **cancels** the task. A queued-but-unrun task is
  dropped without running; a currently-running task completes but
  its result is discarded. Previously the task would run to
  completion regardless. This matches `tokio::JoinHandle` and
  `async_task::Task` semantics.

- The concrete future type returned by `Threadpool::spawn_local`
  changed from `SpawnFuture<'pool, F, R>` to `SpawnFuture<'pool, R>`
  (the closure type parameter was dropped — the closure now lives
  inside an `async_task::Task<R>`). Callers that named the type in
  a `where` clause, stored it in a struct, or returned it from a
  function will need to drop the `F` parameter. Callers that only
  used the returned value as a `Future` (the common case) are
  unaffected.

- `spawn_local` keeps its pre-rewrite lazy-schedule semantic: the
  runnable is pushed onto the queue on first poll of the returned
  `SpawnFuture`, not at the call site. Constructing and dropping a
  `SpawnFuture` without ever polling it is a no-op and never touches
  a worker — required so a 1-worker pool cannot deadlock when the
  only worker does `let _ = pool.spawn_local(...)`. Only `spawn`
  (which has no `'pool` borrow and no drop-blocking contract) was
  switched to eager scheduling.

### Internal changes

- Task allocation, refcounting, completion state machine, and waker
  storage are now delegated to `async-task`.
- Deleted modules: `job`, `task`, `atomic_waker`.
- Deleted tests: the previous `tests/loom.rs` (it modelled the
  removed `AtomicWaker` and `Job<F, R>` protocols). Replaced by
  `tests/loom_queue.rs`, which models the new arm-then-rescan park
  handshake in `src/queue.rs`.
- New module: `src/cpu.rs` — thread-local cached `current_cpu()`
  lookup for shard routing.
- Dependencies: added `async-task`; removed `arc-swap`, `crossbeam`.
- CI: miri (scoped to `--lib` and `tests/async_task_smoke`) and
  loom (`tests/loom_queue`) jobs cover the remaining `unsafe`
  surface and the queue handshake respectively.

### Migration

```rust
// Before (0.5.0):
let h = pool.spawn(|| compute());
// ... h is unscheduled until polled.
// std::mem::drop(h) ran the closure to completion.

// After (this PR):
let h = pool.spawn(|| compute());
// ... the closure is already running on a worker.
// std::mem::drop(h) cancels the task.

// If you previously relied on "fire and forget" via drop:
let h = pool.spawn(|| compute());
std::mem::forget(h); // explicit: keep running, discard result
// or: tokio::spawn(async move { h.await; }); // detach on tokio
```
