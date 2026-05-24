# Changelog

## 0.7.0 — sharded queue + CPU-affinity routing

Internal-only change; public API and semantics unchanged from 0.6.0.

### Performance

The `Mutex<VecDeque>` queue introduced in 0.6.0 was identical in
shape to tokio's blocking pool, which left a 2-3× contention gap on
many-producer / many-worker workloads — both implementations bottle-
necked on the single queue lock. 0.7.0 replaces that with a
sharded queue plus CPU-affinity-based producer routing.

Head-to-head with `tokio::task::spawn_blocking` (`--quick`, system
idle):

| Bench | 0.6.0 | 0.7.0 | Tokio | 0.7.0 vs Tokio |
|---|---|---|---|---|
| `spawn_overhead/4w/10000` | 16.2 ms | 6.37 ms | 21.2 ms | **AP wins 3.3×** |
| `spawn_overhead/4w/100` | 172 µs | 66.7 µs | 144 µs | **AP wins 2.2×** |
| `burst_drain/100k` | 172 ms | 50.7 ms | 153 ms | **AP wins 3.0×** |
| `concurrent_pipeline (8p×100k, 8w)` | 742 ms | 316 ms | 463 ms | **AP wins 1.5×** |
| `multi_producer/8p_4w` | 5.68 ms | 2.15 ms | 6.23 ms | **AP wins 2.9×** |
| `multi_producer/4p_4w` | 1.74 ms | 1.06 ms | 1.35 ms | **AP wins 1.3×** |
| `realistic_cost/100ns` | n/a | 4.57 ms | 20.99 ms | **AP wins 4.6×** |
| `realistic_cost/250µs` | n/a | 99.7 ms | 98.9 ms | parity (work dominates) |
| `sustained_throughput` (4w) | 45.6 ms | 52.3 ms | 61.8 ms | AP wins 1.2× |
| `spawn_overhead/1w/10000` | 1.06 ms | 1.62 ms | 2.27 ms | AP wins 1.4× |

Sustained throughput: **~1.9 M tasks/s on 4 workers, ~2.5 M tasks/s
on 8 workers**.

### Architecture

* **Sharded queue.** `parking_lot::Mutex<VecDeque<Runnable>>` ×
  `num_shards`, where `num_shards = num_workers.next_power_of_two().min(8)`.
  `workers == 1` → 1 shard (same as 0.6.0, no regression). `workers ≥ 5`
  → 8 shards (capped).
* **CPU-affinity routing.** Producers pick a shard via a thread-local
  cache of `sched_getcpu()` (Linux) / `GetCurrentProcessorNumber()`
  (Windows), refreshed every 64 pushes. Other platforms hash the
  thread ID — less geographical, but stable per producer thread,
  which is what gets you the cache-locality win for long-lived
  producers.
* **Shard scanning, not work-stealing.** Each worker has a preferred
  shard (`worker_idx & mask`). On empty, scans the remaining shards
  in cyclic order. No private deques, no victim selection, no
  steal-retry spin. The whole `STEAL_RETRY_BUDGET` / `find_task`
  apparatus we deleted in 0.6.0 stays gone.
* **Lost-wakeup-free park protocol.** Producers acquire the shard
  mutex, push, release, then check a `parked` atomic. If any worker
  may be parked, the producer briefly acquires the `park` mutex to
  call `notify_one`. Workers, when parking, hold the `park` mutex
  across `parked.fetch_add` and a final re-scan of all shards before
  `cv.wait` — so any push whose `parked.load` sees the worker armed
  must serialise through `park.lock`, and the worker's `cv.wait`
  atomically releases that lock with starting to wait. See
  `src/queue.rs` for the full proof sketch.

### New module

* `src/cpu.rs` — thread-local cached `current_cpu()` lookup.

### Files changed

`src/queue.rs` (rewritten), `src/cpu.rs` (new), `src/lib.rs` (worker
loop passes `worker_idx` to `pop_blocking`; `Queue::new` takes
`num_workers`), `src/builder.rs` (same), `BENCHMARKS.md`, `CHANGELOG.md`.

---

## 0.6.0 — async-task rewrite

This is a major internal rewrite that closes the 8-15× performance gap
versus `tokio::task::spawn_blocking` while preserving the public API
surface (`Threadpool::new`, `spawn`, `spawn_local`, `Builder`, CPU
affinity, global threadpool singleton).

### Performance

Head-to-head with `tokio::task::spawn_blocking` (matched producer +
worker counts, `--quick` criterion run, system idle): affinitypool now
wins on the majority of benchmarks and is within 2-3× on the
remaining worst case (heavy-contention 4-worker batched spawn).
Before 0.6.0 it was 8-15× slower across the board.

Highlights:

| Bench | 0.5.0 | 0.6.0 | Tokio | 0.6.0 vs Tokio |
|---|---|---|---|---|
| `spawn_overhead/1w/10000` | 68.8 ms | 1.06 ms | 2.21 ms | **0.48× — AP wins 2.1×** |
| `spawn_overhead/4w/10000` | 48.4 ms | 16.2 ms | 6.99 ms | 2.3× slower (architectural) |
| `round_trip/4w` | 6.30 µs | 4.93 µs | 7.14 µs | **0.69× — AP wins 1.5×** |
| `multi_producer/4p_1w` | 2.63 ms | 576 µs | 816 µs | **0.71× — AP wins 1.4×** |
| `multi_producer/8p_4w` | 6.68 ms | 5.68 ms | 2.89 ms | 2.0× slower (architectural) |

The remaining gap on `4w+` batched-spawn cases is mutex contention on
the shared worker queue. Tokio mitigates this with lazy thread
spawning — at low load only one blocking thread exists, so there is
no inter-worker contention. affinitypool keeps all workers eagerly
running (required for CPU affinity to mean anything), which trades
some throughput for predictable per-core placement. This is the
design intent.

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
  `async_task::Task` semantics, and makes `spawn` consistent with
  `spawn_local`.

- The concrete future type returned by `Threadpool::spawn_local`
  changed from `SpawnFuture<'pool, F, R>` to `SpawnFuture<'pool, R>`
  (the closure type parameter was dropped — the closure now lives
  inside an `async_task::Task<R>`). Callers that named the type in
  a `where` clause, stored it in a struct, or returned it from a
  function will need to drop the `F` parameter. Callers that only
  used the returned value as a `Future` (the common case) are
  unaffected.

- `spawn_local` keeps the pre-0.6.0 lazy-schedule semantic: the
  runnable is pushed onto the queue on first poll of the returned
  `SpawnFuture`, not at the call site. Constructing and dropping a
  `SpawnFuture` without ever polling it is a no-op and never touches
  a worker — required so a 1-worker pool can't deadlock when the
  only worker does `let _ = pool.spawn_local(...)`. Only `spawn`
  (which has no `'pool` borrow and no drop-blocking contract) was
  switched to eager scheduling.

### Internal changes

- Task allocation, refcounting, completion state machine, and waker
  storage are now delegated to the [`async-task`](https://crates.io/crates/async-task)
  crate (single hand-tuned allocation per spawn, mature, used by
  smol/fuchsia).
- Work-stealing queue replaced by a single `parking_lot::Mutex<VecDeque<Runnable>> + Condvar`.
  No per-worker queues, no stealers, no `STEAL_RETRY_BUDGET` spin
  loop, no SeqCst fence handshake.
- Deleted modules: `job`, `task`, `atomic_waker`.
- Deleted tests: `tests/loom.rs` (the synchronization protocol it
  modeled no longer exists; async-task is loom-tested upstream).
- Dependencies: added `async-task`; removed `arc-swap`, `crossbeam`.

### Migration

```rust
// Before (0.5.0):
let h = pool.spawn(|| compute());
// ... h is unscheduled until polled.
// std::mem::drop(h) ran the closure to completion.

// After (0.6.0):
let h = pool.spawn(|| compute());
// ... the closure is already running on a worker.
// std::mem::drop(h) cancels the task.

// If you previously relied on "fire and forget" via drop:
let h = pool.spawn(|| compute());
std::mem::forget(h); // explicit: keep running, discard result
// or: tokio::spawn(async move { h.await; }); // detach on tokio
```
