# Changelog

## Unreleased — async-task rewrite + sharded queue

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
