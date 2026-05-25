# affinitypool

A threadpool for running blocking jobs on a dedicated thread pool. Blocking tasks can be sent asynchronously to the pool, where the task will be queued until a worker thread is free to process the task. Tasks are processed in a FIFO order.

For optimised workloads, the affinity of each thread can be specified, ensuring that each thread can request to be pinned to a certain CPU core, allowing for more parallelism, and better performance guarantees for blocking workloads.

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

### CPU Affinity

Pin each worker thread to a specific CPU core for optimal performance:

```rust
use affinitypool::Builder;

#[tokio::main]
async fn main() {
    // Create a pool with one thread per CPU core, each pinned to its respective core
    let pool = Builder::new()
        .thread_per_core(true)
        .build();
    
    // Tasks will be distributed across CPU cores
    for i in 0..100 {
        pool.spawn(move || {
            println!("Task {i} running on dedicated CPU core");
            // Perform CPU-intensive work with better cache locality
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

Use `spawn_local` when you need to borrow data without the 'static lifetime requirement:

```rust
use affinitypool::Threadpool;

#[tokio::main]
async fn main() {
    let pool = Threadpool::new(4);
    
    let data = vec![1, 2, 3, 4, 5];
    let multiplier = 10;
    
    // spawn_local allows borrowing local data
    let result = pool.spawn_local(|| {
        data.iter()
            .map(|x| x * multiplier)
            .collect::<Vec<_>>()
    }).await;
    
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

Three workloads run against each pool: `spawn_overhead` (submit N closures, await each), `round_trip` (submit-and-await one closure at a time), and `multi_producer` (P concurrent producers each pushing 1k tasks). Numbers are criterion midpoint estimates from `--quick` runs on a quiet Linux bench machine. **Bold** = affinitypool is the fastest in the row.

| Benchmark | affinitypool | tokio | blocking† | rayon | threadpool |
|---|---|---|---|---|---|
| `spawn_overhead/1w/1` | 5.84 µs | 6.36 µs | 2.49 µs | 1.03 µs | 5.76 µs |
| `spawn_overhead/4w/1` | 6.77 µs | 5.71 µs | 2.49 µs | 1.53 µs | 6.54 µs |
| `spawn_overhead/1w/100` | **18.0 µs** | 35.7 µs | 181 µs | 20.7 µs | 27.1 µs |
| `spawn_overhead/4w/100` | 98.8 µs | 87.3 µs | 181 µs | 39.5 µs | 27.2 µs |
| `spawn_overhead/1w/1000` | **208 µs** | 286 µs | 1.86 ms | 270 µs | 278 µs |
| `spawn_overhead/4w/1000` | 875 µs | 1.19 ms | 1.86 ms | 332 µs | 240 µs |
| `spawn_overhead/1w/10000` | 2.26 ms | 2.40 ms | 21.3 ms | 2.20 ms | 2.09 ms |
| `spawn_overhead/4w/10000` | 12.0 ms | 10.4 ms | 21.3 ms | 3.37 ms | 2.19 ms |
| `round_trip/1w` | 5.54 µs | 5.76 µs | 2.49 µs | 1.01 µs | 4.45 µs |
| `round_trip/4w` | 6.19 µs | 6.57 µs | 2.49 µs | 1.44 µs | 6.24 µs |
| `round_trip/8w` | 6.56 µs | 5.37 µs | 2.49 µs | 2.24 µs | 6.78 µs |
| `multi_producer/2p_1w` | **367 µs** | 409 µs | 4.88 ms | 524 µs | 385 µs |
| `multi_producer/2p_4w` | 1.12 ms | 1.73 ms | 4.88 ms | 731 µs | 426 µs |
| `multi_producer/4p_1w` | 1.00 ms | 1.16 ms | 11.2 ms | 973 µs | 890 µs |
| `multi_producer/4p_4w` | 1.16 ms | 1.80 ms | 11.2 ms | 938 µs | 976 µs |
| `multi_producer/8p_1w` | 3.97 ms | 3.29 ms | 27.3 ms | 1.94 ms | 2.35 ms |
| `multi_producer/8p_4w` | **1.61 ms** | 5.42 ms | 27.3 ms | 1.73 ms | 1.94 ms |

† `blocking` uses a single auto-scaled global pool; its column doesn't vary with the worker count.

### How affinitypool compares

* **vs `tokio::spawn_blocking`** — affinitypool wins or ties on most workloads, including a 3.4× lead on `multi_producer/8p_4w`. Tokio still leads on the single-task `4w` cases.
* **vs `blocking::unblock`** — affinitypool dominates batched workloads (5–14× faster) and loses single-task latency (`blocking` is consistently ~2.5 µs). Trade-off: `blocking`'s pool grows unboundedly and is shared globally with any other crate using it.
* **vs `rayon::ThreadPool::spawn`** — Rayon's lock-free deques win single-task latency (3–6×) and most `4w` batched workloads. affinitypool wins on `multi_producer/8p_4w` and ties or wins most `1w` workloads. Rayon is built for work-stealing CPU parallelism, not async producer / worker handoff.
* **vs `threadpool::ThreadPool::execute`** — the original. Parity on `1w` workloads, threadpool wins big (4–5×) on `4w` batched spawn (no sharding overhead), affinitypool wins on `multi_producer/8p_4w` and `2p_1w`.

The pattern: affinitypool loses to the single-queue and work-stealing alternatives on `4w/N` batched-spawn workloads where one producer fans out fast and pays for crossing shards. It wins where shard locality pays back — `1w` (no scan cost), and multi-producer workloads where each producer naturally lands on a different shard via the CPU cache.

Unlike any of these alternatives, affinitypool gives you a dedicated pool sized for blocking work and preserves **CPU affinity** — the feature this library exists for.

## Architecture

Tasks are delivered from producers to workers through a sharded MPMC queue. Each producer routes to a shard via a thread-local cache of its current CPU (`sched_getcpu()` on Linux, `GetCurrentProcessorNumber()` on Windows), so a producer running on core *N* consistently lands on shard `N & mask`. Each worker has a preferred shard (`worker_idx & mask`) and falls back to scanning the remaining shards in cyclic order before parking — there are no private deques and no work-stealing handshake.

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
Worker threads (optionally pinned)
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

**Worker self-spawn fast path.** When a closure running on a worker thread calls `pool.spawn(...)`, the new task is pushed directly into that worker's own local deque instead of routing through the shared sharded queue. The producer (this thread) is also the consumer, so no cross-thread wake is issued — *other workers currently parked stay parked*. This is intentional and saves a wake roundtrip, but a workload that fans out **only** via self-spawn cascades with no external producer to wake parked peers will serialise on the spawning worker until something else wakes them. The spawning worker's local deque is still a stealer target, so peers that wake on a later foreign push will recover the imbalance.

#### Original

This code is heavily inspired by [threadpool](https://crates.io/crates/threadpool), with the CPU-based affinity code forked originally from [core-affinity](https://crates.io/crates/core_affinity). Both are licensed under the Apache License 2.0 and MIT licenses.
