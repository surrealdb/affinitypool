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

#### Original

This code is heavily inspired by [threadpool](https://crates.io/crates/threadpool), with the CPU-based affinity code forked originally from [core-affinity](https://crates.io/crates/core_affinity). Both are licensed under the Apache License 2.0 and MIT licenses.
