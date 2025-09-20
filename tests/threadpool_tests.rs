use affinitypool::{Builder, Threadpool};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[tokio::test]
async fn test_basic_task_execution() {
	let pool = Threadpool::new(4);

	let result = pool.spawn(|| 42).await;
	assert_eq!(result, 42);

	let result = pool.spawn(|| "hello world").await;
	assert_eq!(result, "hello world");
}

#[tokio::test]
async fn test_multiple_tasks() {
	let pool = Threadpool::new(4);
	let counter = Arc::new(AtomicUsize::new(0));

	let mut handles = Vec::new();
	for _ in 0..100 {
		let counter = counter.clone();
		handles.push(pool.spawn(move || {
			counter.fetch_add(1, Ordering::SeqCst);
		}));
	}

	for handle in handles {
		handle.await;
	}

	assert_eq!(counter.load(Ordering::SeqCst), 100);
}

#[tokio::test]
async fn test_work_distribution() {
	let pool = Threadpool::new(8);
	let thread_ids = Arc::new(Mutex::new(Vec::new()));

	let mut handles = Vec::new();
	for _ in 0..100 {
		let thread_ids = thread_ids.clone();
		handles.push(pool.spawn(move || {
			let id = thread::current().id();
			thread_ids.lock().unwrap().push(id);
			// Simulate some work
			thread::sleep(Duration::from_micros(10));
		}));
	}

	for handle in handles {
		handle.await;
	}

	let ids = thread_ids.lock().unwrap();
	let unique_threads: std::collections::HashSet<_> = ids.iter().collect();

	// Should have used multiple threads (but not necessarily all 8)
	assert!(unique_threads.len() > 1);
	println!("Tasks distributed across {} threads", unique_threads.len());
}

#[tokio::test]
async fn test_heavy_computational_load() {
	let pool = Threadpool::new(4);
	let mut results = Vec::new();

	for i in 0..50 {
		results.push(pool.spawn(move || {
			// Simulate heavy computation
			let mut sum = 0u64;
			for j in 0..100_000 {
				sum = sum.wrapping_add((i * j) as u64);
			}
			sum
		}));
	}

	let mut total = 0u64;
	for result in results {
		total = total.wrapping_add(result.await);
	}

	// Just verify all tasks completed
	assert!(total > 0);
}

#[tokio::test]
async fn test_global_threadpool() {
	// Create a new global threadpool
	let pool = Threadpool::new(4);
	assert!(pool.build_global().is_ok());

	// Use global spawn
	let result = affinitypool::spawn(|| {
		thread::sleep(Duration::from_millis(10));
		123
	})
	.await;

	assert_eq!(result, 123);
}

#[tokio::test]
async fn test_spawn_local() {
	let pool = Threadpool::new(4);

	// Test that spawn_local works with local borrowing
	let data = vec![1, 2, 3, 4, 5];
	let result = pool.spawn_local(|| data.iter().sum::<i32>()).await;

	assert_eq!(result, 15);
	// Verify we can still use data after spawn_local
	assert_eq!(data.len(), 5);
}

#[tokio::test]
async fn test_panic_recovery() {
	let pool = Threadpool::new(4);
	let initial_thread_count = pool.thread_count();

	// Spawn a task that panics
	let result = pool.spawn(|| {
		panic!("Test panic!");
	});

	// The panic should be caught and re-thrown
	let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
		tokio::runtime::Runtime::new().unwrap().block_on(result)
	}));

	assert!(panic_result.is_err());

	// Give the pool time to spawn a replacement thread
	thread::sleep(Duration::from_millis(100));

	// Thread pool should still be functional
	let result = pool.spawn(|| 42).await;
	assert_eq!(result, 42);

	// Thread count should be maintained
	assert_eq!(pool.thread_count(), initial_thread_count);
}

#[tokio::test]
async fn test_builder_configuration() {
	// Test with custom number of threads
	let pool = Builder::new().worker_threads(2).build();

	assert_eq!(pool.num_threads(), 2);

	let result = pool.spawn(|| 42).await;
	assert_eq!(result, 42);
}

#[tokio::test]
async fn test_thread_naming() {
	let pool = Builder::new().worker_threads(2).thread_name("test-worker").build();

	let thread_name = pool.spawn(|| thread::current().name().unwrap().to_string()).await;

	// Thread names should contain the base name
	assert!(thread_name.contains("test-worker"));
}

#[tokio::test]
async fn test_concurrent_spawns() {
	let pool = Arc::new(Threadpool::new(4));
	let mut handles = Vec::new();

	// Spawn many tasks concurrently from multiple tokio tasks
	for i in 0..10 {
		let pool = pool.clone();
		let handle = tokio::spawn(async move {
			let mut results = Vec::new();
			for j in 0..10 {
				let result = pool.spawn(move || i * 10 + j).await;
				results.push(result);
			}
			results
		});
		handles.push(handle);
	}

	let mut all_results = Vec::new();
	for handle in handles {
		let results = handle.await.unwrap();
		all_results.extend(results);
	}

	all_results.sort();
	let expected: Vec<i32> = (0..100).collect();
	assert_eq!(all_results, expected);
}

#[tokio::test]
async fn test_work_stealing_efficiency() {
	let pool = Threadpool::new(4);
	let start = Instant::now();

	// Create a mix of fast and slow tasks
	let mut all_results = Vec::new();

	// Some quick tasks
	let mut quick_handles1 = Vec::new();
	for i in 0..50 {
		quick_handles1.push(pool.spawn(move || i * 2));
	}

	// Some slower tasks
	let mut slow_handles = Vec::new();
	for i in 0..10 {
		slow_handles.push(pool.spawn(move || {
			thread::sleep(Duration::from_millis(10));
			i * 3
		}));
	}

	// More quick tasks
	let mut quick_handles2 = Vec::new();
	for i in 0..50 {
		quick_handles2.push(pool.spawn(move || i * 4));
	}

	// Collect all results
	for handle in quick_handles1 {
		all_results.push(handle.await);
	}
	for handle in slow_handles {
		all_results.push(handle.await);
	}
	for handle in quick_handles2 {
		all_results.push(handle.await);
	}

	let elapsed = start.elapsed();

	// All tasks should complete
	assert_eq!(all_results.len(), 110);

	// Should be reasonably efficient (not waiting for all slow tasks sequentially)
	// With 4 threads and 10 tasks of 10ms each, optimal would be ~30ms
	// Allow for some overhead
	assert!(elapsed < Duration::from_millis(200), "Took too long: {:?}", elapsed);
}

#[tokio::test]
async fn test_zero_tasks() {
	let pool = Threadpool::new(4);
	// Pool should handle having no tasks gracefully
	assert_eq!(pool.thread_count(), 4);
	assert_eq!(pool.num_threads(), 4);
}

#[tokio::test]
async fn test_single_thread_pool() {
	let pool = Threadpool::new(1);

	let mut results = Vec::new();
	for i in 0..10 {
		results.push(pool.spawn(move || i).await);
	}

	assert_eq!(results, (0..10).collect::<Vec<_>>());
}

#[tokio::test]
#[should_panic(expected = "assertion failed")]
async fn test_zero_threads_panics() {
	// This should panic as per the Builder implementation
	Builder::new().worker_threads(0).build();
}

#[tokio::test]
async fn test_large_return_values() {
	let pool = Threadpool::new(4);

	let large_vec = pool
		.spawn(|| {
			vec![1u8; 1_000_000] // 1MB vector
		})
		.await;

	assert_eq!(large_vec.len(), 1_000_000);
	assert!(large_vec.iter().all(|&x| x == 1));
}

#[tokio::test]
async fn test_nested_spawns() {
	let pool = Arc::new(Threadpool::new(4));

	let pool_clone = pool.clone();
	let result = pool
		.spawn(move || {
			// Can't await inside a blocking task, but we can queue another task
			let pool = pool_clone;
			std::thread::spawn(move || {
				let rt = tokio::runtime::Runtime::new().unwrap();
				rt.block_on(async { pool.spawn(|| 42).await })
			})
			.join()
			.unwrap()
		})
		.await;

	assert_eq!(result, 42);
}
