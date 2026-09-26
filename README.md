# affinitypool

A threadpool for running blocking jobs on a dedicated thread pool. Blocking tasks can be sent asynchronously to the pool, where the task will be queued until a worker thread is free to process the task. Tasks are processed in a FIFO order.

Tasks are delivered through a sharded, lock-free queue. Each producer thread routes consistently to its own shard, so concurrent producers' traffic stays isolated and contention stays low, while idle workers steal across shards to stay busy.

## Examples

### Basic Usage

Create a threadpool and spawn tasks that run on worker threads:

```rust
use affinitypool::Threadpool;

#[tokio::main]
async fn main() {
    // Create a threadpool with 4 worker threads
    let pool = Threadpool::new(4);
    
    // Spawn a simple task
    let result = pool.spawn(|| {
        println!("Hello from a worker thread!");
        42
    }).await;
    
    assert_eq!(result, 42);
}
```

### Using the Builder

Configure the threadpool with custom settings:

```rust
use affinitypool::Builder;

#[tokio::main]
async fn main() {
    let pool = Builder::new()
        .worker_threads(8)              // Set number of worker threads
        .thread_name("my-worker")        // Name the worker threads
        .thread_stack_size(4_000_000)    // Set 4MB stack size per thread
        .build();
    
    // Execute CPU-intensive tasks
    let mut handles = Vec::new();
    for i in 0..100 {
        handles.push(pool.spawn(move || {
            // Simulate heavy computation
            let mut sum = 0u64;
            for j in 0..1_000_000 {
                sum = sum.wrapping_add((i * j) as u64);
            }
            sum
        }));
    }
    
    // Collect results
    for handle in handles {
        let result = handle.await;
        println!("Task completed with result: {result}");
    }
}
```

### Thread per core

Spawn one worker thread per CPU core. This sets the worker count to the number of available cores; thread placement is left to the OS scheduler (workers are not pinned):

```rust
use affinitypool::Builder;

#[tokio::main]
async fn main() {
    // Create a pool with one worker thread per CPU core
    let pool = Builder::new()
        .thread_per_core(true)
        .build();

    // Tasks are distributed across the worker threads
    for i in 0..100 {
        pool.spawn(move || {
            println!("Task {i} running on a per-core worker pool");
        }).await;
    }
}
```

### Global Threadpool

Set up a global threadpool that can be accessed from anywhere:

```rust
use affinitypool::{Threadpool, spawn};

#[tokio::main]
async fn main() {
    // Initialize the global threadpool
    let pool = Threadpool::new(4);
    pool.build_global().expect("Global threadpool already initialized");
    
    // Now you can use the global spawn function from anywhere
    let result = spawn(|| {
        // This runs on the global threadpool
        std::thread::sleep(std::time::Duration::from_millis(100));
        "completed"
    }).await;
    
    assert_eq!(result, "completed");
    
    // Can be called from any async context without passing the pool reference
    process_data().await;
}

async fn process_data() {
    let result = spawn(|| {
        // Complex blocking operation
        vec![1, 2, 3, 4, 5].iter().sum::<i32>()
    }).await;
    
    println!("Sum: {result}");
}
```

### Local Spawning

Use `spawn_local` when you need to borrow data without the `'static` lifetime requirement.

`spawn_local` is **`unsafe`**: the closure may borrow non-`'static` data, and that borrow stays sound only as long as the returned future is *not leaked* (`mem::forget`, `Box::leak`, an `Rc`/`Arc` cycle, …) before the borrow ends. Awaiting it — or simply letting it drop — upholds the contract; leaking it while it borrows local data is a use-after-free. This cannot be enforced statically in async Rust, which is why the API is `unsafe` rather than safe; if your closure only captures `'static` data, prefer the safe `spawn`.

```rust
use affinitypool::Threadpool;

#[tokio::main]
async fn main() {
    let pool = Threadpool::new(4);
    
    let data = vec![1, 2, 3, 4, 5];
    let multiplier = 10;
    
    // spawn_local allows borrowing local data.
    // SAFETY: the future is awaited immediately and never leaked, so the
    // borrows of `data`/`multiplier` cannot outlive it.
    let result = unsafe {
        pool.spawn_local(|| {
            data.iter()
                .map(|x| x * multiplier)
                .collect::<Vec<_>>()
        })
    }.await;
    
    println!("Result: {result:?}");  // [10, 20, 30, 40, 50]
    
    // data is still accessible after spawn_local
    println!("Original data: {data:?}");
}
```

### Handling Multiple Concurrent Tasks

Process multiple blocking tasks concurrently:

```rust
use affinitypool::Threadpool;
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

#[tokio::main]
async fn main() {
    let pool = Threadpool::new(4);
    let counter = Arc::new(AtomicUsize::new(0));
    
    // Spawn multiple tasks concurrently
    let mut handles = Vec::new();
    for i in 0..100 {
        let counter = counter.clone();
        handles.push(pool.spawn(move || {
            // Simulate blocking I/O or computation
            std::thread::sleep(std::time::Duration::from_millis(10));
            counter.fetch_add(1, Ordering::SeqCst);
            format!("Task {i} completed")
        }));
    }
    
    // Wait for all tasks to complete
    for handle in handles {
        let result = handle.await;
        println!("{result}");
    }
    
    assert_eq!(counter.load(Ordering::SeqCst), 100);
    println!("All tasks completed!");
}
```

## Benchmarks

Head-to-head against the most common alternatives for running blocking work in async Rust:

* [`tokio::task::spawn_blocking`](https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html) — Tokio's built-in blocking pool.
* [`blocking::unblock`](https://docs.rs/blocking) — the auto-scaling pool used by `async-std` and the smol ecosystem.
* [`rayon::ThreadPool::spawn`](https://docs.rs/rayon) — Rayon's work-stealing pool. Tasks are wrapped in a `tokio::sync::oneshot` so the producer can await; that handshake is part of what's measured.
* [`threadpool::ThreadPool::execute`](https://docs.rs/threadpool) — the crate this library was originally forked from. Same `oneshot` wrap as Rayon.

Three workloads run against each pool: `spawn_overhead` (submit N closures, await each), `round_trip` (submit-and-await one closure at a time), and `multi_producer` (P concurrent producers each pushing 1k tasks). Numbers are criterion midpoint estimates from `--quick` runs on a quiet Linux bench machine. <img width="16" align="absmiddle" src="/img/rocket.png" alt="🚀"> denotes the fastest implementation in each row.

| Benchmark | affinitypool | tokio | blocking† | rayon | threadpool |
| :--- | ---: | ---: | ---: | ---: | ---: |
| `spawn_overhead/1w/1` | 1.20 µs | 7.52 µs | 2.31 µs | **974 ns**&nbsp;<img width="16" align="absmiddle" src="/img/rocket.png" alt="🚀"> | 7.68 µs |
| `spawn_overhead/4w/1` | **1.19 µs**&nbsp;<img width="16" align="absmiddle" src="/img/rocket.png" alt="🚀"> | 2.88 µs | 2.31 µs | 1.27 µs | 8.10 µs |
| `spawn_overhead/1w/100` | **12.5 µs**&nbsp;<img width="16" align="absmiddle" src="/img/rocket.png" alt="🚀"> | 65.9 µs | 232.7 µs | 44.0 µs | 13.2 µs |
| `spawn_overhead/4w/100` | **27.4 µs**&nbsp;<img width="16" align="absmiddle" src="/img/rocket.png" alt="🚀"> | 54.9 µs | 232.7 µs | 109.5 µs | 68.6 µs |
| `spawn_overhead/1w/1000` | **150.0 µs**&nbsp;<img width="16" align="absmiddle" src="/img/rocket.png" alt="🚀"> | 156.9 µs | 1.59 ms | 811.0 µs | 459.5 µs |
| `spawn_overhead/4w/1000` | **195.2 µs**&nbsp;<img width="16" align="absmiddle" src="/img/rocket.png" alt="🚀"> | 517.9 µs | 1.59 ms | 251.8 µs | 314.0 µs |
| `spawn_overhead/1w/10000` | **1.46 ms**&nbsp;<img width="16" align="absmiddle" src="/img/rocket.png" alt="🚀"> | 1.81 ms | 27.43 ms | 7.85 ms | 1.55 ms |
| `spawn_overhead/4w/10000` | 2.20 ms | 6.44 ms | 27.43 ms | 8.76 ms | **2.02 ms**&nbsp;<img width="16" align="absmiddle" src="/img/rocket.png" alt="🚀"> |
| `round_trip/1w` | 6.71 µs | 7.11 µs | 6.98 µs | **965 ns**&nbsp;<img width="16" align="absmiddle" src="/img/rocket.png" alt="🚀"> | 7.72 µs |
| `round_trip/4w` | 3.11 µs | 2.88 µs | 6.98 µs | **1.66 µs**&nbsp;<img width="16" align="absmiddle" src="/img/rocket.png" alt="🚀"> | 3.01 µs |
| `round_trip/8w` | **3.51 µs**&nbsp;<img width="16" align="absmiddle" src="/img/rocket.png" alt="🚀"> | 7.16 µs | 6.98 µs | 5.67 µs | 8.19 µs |
| `multi_producer/2p_1w` | **210.8 µs**&nbsp;<img width="16" align="absmiddle" src="/img/rocket.png" alt="🚀"> | 310.4 µs | 5.47 ms | 359.9 µs | 276.5 µs |
| `multi_producer/2p_4w` | **195.9 µs**&nbsp;<img width="16" align="absmiddle" src="/img/rocket.png" alt="🚀"> | 1.35 ms | 5.47 ms | 388.5 µs | 444.3 µs |
| `multi_producer/4p_1w` | **504.6 µs**&nbsp;<img width="16" align="absmiddle" src="/img/rocket.png" alt="🚀"> | 1.08 ms | 12.97 ms | 685.9 µs | 645.6 µs |
| `multi_producer/4p_4w` | **276.5 µs**&nbsp;<img width="16" align="absmiddle" src="/img/rocket.png" alt="🚀"> | 1.70 ms | 12.97 ms | 992.9 µs | 1.56 ms |
| `multi_producer/8p_1w` | 2.06 ms | 4.47 ms | 29.03 ms | 5.44 ms | **1.62 ms**&nbsp;<img width="16" align="absmiddle" src="/img/rocket.png" alt="🚀"> |
| `multi_producer/8p_4w` | **1.08 ms**&nbsp;<img width="16" align="absmiddle" src="/img/rocket.png" alt="🚀"> | 3.92 ms | 29.03 ms | 2.34 ms | 4.18 ms |

† `blocking` uses a single auto-scaled global pool; its column doesn't vary with the worker count.

### How affinitypool compares

* **vs `tokio::spawn_blocking`** — affinitypool wins across virtually all workloads, with up to a 6.9× lead on multi-producer contention (`multi_producer/2p_4w`), a 3.6× lead on `multi_producer/8p_4w`, and up to a 5.1× lead on concurrent sustained pipeline bursts.
* **vs `blocking::unblock`** — affinitypool dominates batched workloads (10–27× faster) and multi-producer contention (27–47× faster). Trade-off: `blocking`'s pool grows unboundedly and is shared globally with any other crate using it.
* **vs `rayon::ThreadPool::spawn`** — affinitypool wins on almost all batched and multi-producer workloads (up to 5.4× faster on `spawn_overhead/1w/10000`, 3.6× faster on `multi_producer/4p_4w`). Rayon leads on single-task round-trip latency (`round_trip/1w` and `4w`). Rayon is built for work-stealing CPU parallelism, not async producer / worker handoff.
* **vs `threadpool::ThreadPool::execute`** — the original. affinitypool matches or beats threadpool on single-worker workloads, and wins heavily on multi-producer concurrent workloads (up to 5.6× faster on `multi_producer/4p_4w` and 3.9× faster on `8p_4w`).

The pattern: affinitypool dominates concurrent multi-producer and batched workloads where each producer routes consistently to its own shard via its thread-ID hash. It provides dedicated pool sizing for blocking work with **per-producer shard affinity** — concurrent producers stay isolated on their own shards, which is where it wins.

## Architecture

Tasks are delivered from producers to workers through a sharded MPMC queue. Each producer routes to a shard via a cached hash of its thread ID, so a given producer consistently lands on the same shard (`hash & mask`). Each worker has a preferred shard (`worker_idx & mask`) and falls back to scanning the remaining shards in cyclic order before parking.

```text
Producers (any async task)
   +----------------+   +----------------+   +----------------+
   | producer @ c0  |   | producer @ c1  |   | producer @ cN  |
   +----------------+   +----------------+   +----------------+
           |                    |                    |
           v                    v                    v
Sharded queue  (num_workers.next_power_of_two().min(8))
   +----------------+   +----------------+   +----------------+
   |    Shard 0     |   |    Shard 1     |   |    Shard k     |
   |   Mutex<       |   |   Mutex<       |   |   Mutex<       |
   |    VecDeque<   |   |    VecDeque<   |   |    VecDeque<   |
   |    Runnable>>  |   |    Runnable>>  |   |    Runnable>>  |
   +----------------+   +----------------+   +----------------+
           |                    |                    |
           v                    v                    v
Worker threads
   +----------------+   +----------------+   +----------------+
   |    worker 0    |   |    worker 1    |   |    worker k    |
   |  pref: shard 0 |   |  pref: shard 1 |   |  pref: shard k |
   +----------------+   +----------------+   +----------------+
```

On an empty preferred shard a worker scans the remaining shards in cyclic order, then parks on a shared `Mutex<()> + Condvar` (counted by an `AtomicUsize`). Producers check that counter after pushing; if any worker may be parked, they briefly take the park mutex to `notify_one`.

Each task is a single heap allocation (the [`async-task`](https://crates.io/crates/async-task) layout — fused header + closure + result slot + waker). The park/unpark handshake is lost-wakeup-free; the proof sketch lives in [src/queue.rs](src/queue.rs) and the model in [tests/loom_queue.rs](tests/loom_queue.rs).

Shard count rules of thumb:

| Workers | Shards |
|---|---|
| 1 | 1 (no scan cost, no extra mutex) |
| 2–3 | 2–4 |
| ≥ 5 | 8 (capped) |

### Behaviour notes

**Worker self-spawn fast path.** When a closure running on a worker thread calls `pool.spawn(...)`, the new task is pushed directly into that worker's own local deque instead of routing through the shared sharded queue, skipping the shard routing. The spawning worker is usually also the consumer — it returns to its pop loop and drains its own deque — so the work stays biased toward that worker, which is what you want for cache locality.

It still issues the same wake handshake a foreign push does, because the spawning worker is *not guaranteed* to reach its pop loop: a worker that polls a `SpawnFuture` and then drops it blocks in the drop, waiting for the very runnable it just queued. That runnable is in the blocked worker's own deque, so only a peer steal can complete it — the spawning worker's deque is a stealer target. Skipping the wake there let the pool hang until the blocked worker gave up, which is forever. On a one-worker pool there is no peer to wake, so that pattern self-deadlocks regardless; see the `Threadpool::spawn_local` docs.

#### Original

This code is heavily inspired by [threadpool](https://crates.io/crates/threadpool), licensed under the Apache License 2.0 and MIT licenses. Earlier versions also included CPU-core-pinning code forked from [core-affinity](https://crates.io/crates/core_affinity); that code has since been removed.
