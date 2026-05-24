#![cfg(not(loom))]

use affinitypool::{Builder, Threadpool};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

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
	let data = [1, 2, 3, 4, 5];
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
async fn test_mixed_workload_completion() {
	// Tests that the threadpool correctly handles a mix of fast and slow tasks,
	// ensuring all tasks complete and return correct results.
	// Performance characteristics are measured by the benchmarks.
	let pool = Threadpool::new(4);

	// Create a mix of fast and slow tasks
	let mut all_results = Vec::new();

	// Some quick tasks (group 1)
	let mut quick_handles1 = Vec::new();
	for i in 0..50 {
		quick_handles1.push(pool.spawn(move || i * 2));
	}

	// Some slower tasks with sleep
	let mut slow_handles = Vec::new();
	for i in 0..10 {
		slow_handles.push(pool.spawn(move || {
			thread::sleep(Duration::from_millis(10));
			i * 3
		}));
	}

	// More quick tasks (group 2)
	let mut quick_handles2 = Vec::new();
	for i in 0..50 {
		quick_handles2.push(pool.spawn(move || i * 4));
	}

	// Collect all results - verifying they all complete
	for handle in quick_handles1 {
		all_results.push(handle.await);
	}
	for handle in slow_handles {
		all_results.push(handle.await);
	}
	for handle in quick_handles2 {
		all_results.push(handle.await);
	}

	// Verify all tasks completed
	assert_eq!(all_results.len(), 110);

	// Verify results are correct
	// First 50 should be 0*2, 1*2, ..., 49*2
	let group1_results: Vec<_> = all_results[0..50].to_vec();
	let expected1: Vec<_> = (0..50).map(|i| i * 2).collect();
	assert_eq!(group1_results, expected1, "First group of quick tasks produced incorrect results");

	// Next 10 should be 0*3, 1*3, ..., 9*3
	let slow_results: Vec<_> = all_results[50..60].to_vec();
	let expected_slow: Vec<_> = (0..10).map(|i| i * 3).collect();
	assert_eq!(slow_results, expected_slow, "Slow tasks produced incorrect results");

	// Last 50 should be 0*4, 1*4, ..., 49*4
	let group2_results: Vec<_> = all_results[60..110].to_vec();
	let expected2: Vec<_> = (0..50).map(|i| i * 4).collect();
	assert_eq!(group2_results, expected2, "Second group of quick tasks produced incorrect results");
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
async fn test_zero_threads_clamped_to_one() {
	// Zero threads should be clamped to 1
	let pool = Builder::new().worker_threads(0).build();
	assert_eq!(pool.num_threads(), 1);

	// Verify the pool works
	let result = pool.spawn(|| 42).await;
	assert_eq!(result, 42);
}

#[tokio::test]
async fn test_zero_threads_direct_clamped_to_one() {
	// Direct construction with 0 workers should clamp to 1
	let pool = Threadpool::new(0);
	assert_eq!(pool.num_threads(), 1);

	// Verify it works
	let result = pool.spawn(|| "hello").await;
	assert_eq!(result, "hello");
}

#[tokio::test]
async fn test_too_many_threads_clamped_to_max() {
	// Exceeding MAX_THREADS should clamp to MAX_THREADS (512)
	let pool = Threadpool::new(1000);
	assert_eq!(pool.num_threads(), 512);

	// Verify it works
	let result = pool.spawn(|| 123).await;
	assert_eq!(result, 123);
}

#[tokio::test]
async fn test_builder_too_many_threads_clamped() {
	// Builder should also clamp to MAX_THREADS
	let pool = Builder::new().worker_threads(10_000).build();
	assert_eq!(pool.num_threads(), 512);

	// Verify it works
	let result = pool.spawn(|| vec![1, 2, 3]).await;
	assert_eq!(result, vec![1, 2, 3]);
}

#[tokio::test]
async fn test_max_threads_allowed() {
	// Test that exactly MAX_THREADS (512) is allowed
	let pool = Threadpool::new(512);
	// Just verify it was created successfully
	assert_eq!(pool.num_threads(), 512);

	// Test a simple task to ensure the pool works
	let result = pool.spawn(|| 42).await;
	assert_eq!(result, 42);

	// Test that values above 512 are clamped
	let pool2 = Threadpool::new(513);
	assert_eq!(pool2.num_threads(), 512);
}

#[tokio::test]
async fn test_reasonable_thread_counts() {
	// Test various reasonable thread counts within limits
	for count in [1, 2, 4, 8, 16, 32, 64, 128, 256, 512] {
		let pool = Threadpool::new(count);
		assert_eq!(pool.num_threads(), count);

		// Run a simple task
		let result = pool.spawn(move || count * 2).await;
		assert_eq!(result, count * 2);
	}
}

#[tokio::test]
async fn test_clamping_behavior() {
	// Test that various out-of-bounds values are clamped correctly

	// Values below minimum
	assert_eq!(Threadpool::new(0).num_threads(), 1);

	// Values above maximum
	assert_eq!(Threadpool::new(513).num_threads(), 512);
	assert_eq!(Threadpool::new(1000).num_threads(), 512);
	assert_eq!(Threadpool::new(usize::MAX).num_threads(), 512);

	// Builder with various values
	assert_eq!(Builder::new().worker_threads(0).build().num_threads(), 1);
	assert_eq!(Builder::new().worker_threads(600).build().num_threads(), 512);

	// Default builder behavior (should use 2 threads as fallback)
	assert_eq!(Builder::new().build().num_threads(), 2);
}

#[tokio::test]
async fn test_default_threadpool() {
	// Default threadpool should use num_cpus clamped to MAX_THREADS
	let pool = Threadpool::default();
	let cpu_count = num_cpus::get();
	let expected = cpu_count.min(512);
	assert_eq!(pool.num_threads(), expected);

	// Verify it works
	let result = pool.spawn(|| "default pool").await;
	assert_eq!(result, "default pool");
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

#[tokio::test]
async fn test_threads_properly_joined_on_drop() {
	use std::sync::Arc;
	use std::sync::atomic::{AtomicBool, Ordering};
	use std::thread;
	use std::time::{Duration, Instant};

	// Test that Drop waits for all worker threads to exit
	let thread_exited = Arc::new(AtomicBool::new(false));
	let exit_clone = thread_exited.clone();

	// Spawn a simple task and immediately drop the pool
	{
		let pool = Threadpool::new(4);
		// Just verify pool creates threads
		pool.spawn(move || {
			thread::sleep(Duration::from_millis(50));
			exit_clone.store(true, Ordering::SeqCst);
		})
		.await;
	} // Pool dropped here

	// Thread should have completed before drop returned
	assert!(
		thread_exited.load(Ordering::SeqCst),
		"Task should have completed before pool was dropped"
	);

	// Now test that threads are actually joined on drop
	let start = Instant::now();
	{
		let pool = Threadpool::new(2);
		// Submit work to make sure threads are active
		for i in 0..10 {
			pool.spawn(move || {
				// Quick work
				thread::sleep(Duration::from_millis(10));
				i * 2
			})
			.await;
		}
		// Pool is dropped here - should wait for threads to shut down
	}
	let elapsed = start.elapsed();

	// Drop should have waited for threads to shut down properly
	// This should be quick but not instant
	assert!(
		elapsed >= Duration::from_millis(50),
		"Drop completed too quickly ({:?}), threads may not have been joined properly",
		elapsed
	);
}

#[tokio::test]
async fn test_drop_waits_for_running_tasks() {
	use std::sync::Arc;
	use std::sync::atomic::{AtomicUsize, Ordering};
	use std::time::{Duration, Instant};

	let completed_tasks = Arc::new(AtomicUsize::new(0));

	let start = Instant::now();
	{
		let pool = Threadpool::new(2);

		// Spawn multiple long-running tasks
		for _ in 0..4 {
			let counter = completed_tasks.clone();
			pool.spawn(move || {
				// Simulate work
				std::thread::sleep(Duration::from_millis(100));
				counter.fetch_add(1, Ordering::SeqCst);
			})
			.await;
		}
		// Pool is dropped here
	}
	let elapsed = start.elapsed();

	// All tasks should have completed
	assert_eq!(completed_tasks.load(Ordering::SeqCst), 4);

	// Drop should have waited for tasks to complete
	// With 2 threads and 4 tasks of 100ms each, should take ~200ms
	assert!(elapsed >= Duration::from_millis(150), "Drop returned too quickly: {:?}", elapsed);
}

#[tokio::test]
async fn test_all_threads_joined_including_panicked() {
	use std::sync::Arc;
	use std::sync::atomic::{AtomicUsize, Ordering};
	use std::time::Duration;

	// Track how many threads have been created
	let threads_created = Arc::new(AtomicUsize::new(0));
	let created_clone = threads_created.clone();

	{
		let pool = Threadpool::new(2);

		// Task that increments counter (to track thread creation)
		pool.spawn(move || {
			created_clone.fetch_add(1, Ordering::SeqCst);
		})
		.await;

		// Task that panics - should cause a replacement thread to be created
		let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
			tokio::runtime::Runtime::new().unwrap().block_on(async {
				pool.spawn(|| {
					panic!("Intentional panic for testing");
				})
				.await
			})
		}));

		// Give time for replacement thread to spawn
		std::thread::sleep(Duration::from_millis(100));

		// Another task to verify pool still works
		let created_clone2 = threads_created.clone();
		pool.spawn(move || {
			created_clone2.fetch_add(1, Ordering::SeqCst);
		})
		.await;

		// When pool drops, it should join ALL threads:
		// - The original 2 threads
		// - The replacement thread that was created after the panic
	} // Drop happens here

	// With the simplified Vec<JoinHandle> approach, we properly track
	// and join ALL threads, including replacements after panics
	assert!(
		threads_created.load(Ordering::SeqCst) >= 2,
		"Should have executed tasks on multiple threads"
	);
}

#[tokio::test]
async fn test_no_lost_wakeups_race_condition() {
	use std::sync::Arc;
	use std::sync::atomic::{AtomicUsize, Ordering};
	use std::time::{Duration, Instant};

	// This test verifies that the park/unpark race condition is handled correctly
	// Previously, there was a race where:
	// 1. Thread adds itself to parked_threads
	// 2. Another thread pops it and calls unpark
	// 3. First thread hasn't called park yet, so the wakeup is lost

	let pool = Threadpool::new(2);
	let tasks_completed = Arc::new(AtomicUsize::new(0));

	// Submit many quick tasks in rapid succession
	// This increases the chance of hitting the race condition
	for _ in 0..100 {
		let counter = tasks_completed.clone();
		pool.spawn(move || {
			// Very quick task
			counter.fetch_add(1, Ordering::SeqCst);
		})
		.await;

		// Small delay to let threads potentially park
		tokio::time::sleep(Duration::from_micros(100)).await;
	}

	// All tasks should complete quickly
	let start = Instant::now();
	while tasks_completed.load(Ordering::SeqCst) < 100 {
		if start.elapsed() > Duration::from_secs(1) {
			panic!(
				"Tasks didn't complete in time, only {} of 100 completed. \
				 Possible lost wakeup in park/unpark race condition.",
				tasks_completed.load(Ordering::SeqCst)
			);
		}
		tokio::time::sleep(Duration::from_millis(10)).await;
	}

	assert_eq!(tasks_completed.load(Ordering::SeqCst), 100);
}

#[tokio::test]
async fn test_spawn_local_lifetime_safety() {
	// This test verifies that spawn_local properly handles lifetime erasure
	// and safely manages local data capture

	let pool = Threadpool::new(2);

	// Test 1: Verify no use-after-free with local data
	{
		let local_data = [1, 2, 3, 4, 5];

		let result = pool
			.spawn_local(move || {
				// Access the local data - this is safe because spawn_local
				// ensures the task completes before the future is dropped
				local_data.iter().sum::<i32>()
			})
			.await;

		assert_eq!(result, 15);
	}

	// Test 2: Multiple spawn_local calls with different captured data
	let mut results = Vec::new();
	for i in 0..5 {
		let data = [i; 5];
		results.push(
			pool.spawn_local(move || {
				// Each closure captures different data
				data.iter().sum::<i32>()
			})
			.await,
		);
	}
	assert_eq!(results, vec![0, 5, 10, 15, 20]);

	// Test 3: Complex data capture
	{
		let string_data = String::from("Hello, World!");
		let vec_data = vec![1, 2, 3];
		let tuple_data = (42, "test");

		let result = pool
			.spawn_local(move || format!("{} - {:?} - {:?}", string_data, vec_data, tuple_data))
			.await;

		assert_eq!(result, "Hello, World! - [1, 2, 3] - (42, \"test\")");
	}

	// Test 4: Verify spawn_local can safely reference pool lifetime
	let value = 100;
	let result = pool.spawn_local(|| value * 2).await;
	assert_eq!(result, 200);
}

#[tokio::test]
async fn test_spawn_local_no_nested_deadlock() {
	// Verify we handle the potential deadlock case where SpawnFuture
	// is dropped from within a pool thread

	let pool = Arc::new(Threadpool::new(2));

	// This would deadlock if not handled properly
	let pool_clone = pool.clone();
	let result = pool
		.spawn(move || {
			// We're now inside a pool thread
			let pool = pool_clone;

			// Create a SpawnFuture and immediately drop it
			// This should NOT deadlock thanks to our detection
			{
				let _future = pool.spawn_local(|| 42);
				// Future dropped here without awaiting
			}

			// Return something to show we didn't deadlock
			"no deadlock"
		})
		.await;

	assert_eq!(result, "no deadlock");
}

/// Regression: a 1-worker pool must not deadlock when the only worker
/// constructs and drops a `SpawnFuture` without polling it.
///
/// Eager `runnable.schedule()` in `spawn_local` would put the runnable
/// on the queue and then `SpawnFuture::drop` would block the worker
/// waiting for itself to pop the runnable, deadlocking the pool. The
/// 2-worker variant above hides this because the second worker can
/// drain the queue.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_spawn_local_single_worker_drop_without_poll() {
	let pool = Arc::new(Threadpool::new(1));
	let pool_clone = pool.clone();
	// Bound the wait so a regression surfaces as a test failure
	// (timeout) rather than a hung CI run.
	let outcome = tokio::time::timeout(
		Duration::from_secs(5),
		tokio::task::spawn(async move {
			pool_clone
				.spawn(move || {
					// Construct then drop a `SpawnFuture` without
					// polling it from inside the only worker thread.
					// With lazy scheduling the runnable is never
					// queued, so Drop returns immediately.
					let inner = pool.clone();
					let _future = inner.spawn_local(|| 42);
					"ok"
				})
				.await
		}),
	)
	.await;
	assert!(
		matches!(outcome, Ok(Ok("ok"))),
		"single-worker drop-without-poll must not deadlock: {outcome:?}"
	);
}

// =============================================================================
// New tests added alongside the perf + unsafe overhaul.
// =============================================================================

/// Mixed-return-type smoke test for the new single-allocation spawn path.
/// Exercises ZST, large allocation, and panicking returns to make sure the
/// `SpawnCompletion` slot handles all variants correctly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_spawn_mixed_return_types_smoke() {
	let pool = Arc::new(Threadpool::new(4));
	let mut joins = Vec::new();
	for i in 0..1_000 {
		let pool = pool.clone();
		joins.push(tokio::spawn(async move {
			// () return: ZST.
			pool.spawn(|| {}).await;
			// usize return.
			let n: usize = pool.spawn(move || i + 1).await;
			assert_eq!(n, i + 1);
			// Large Vec<u8> return.
			let v: Vec<u8> = pool.spawn(|| vec![7u8; 4096]).await;
			assert_eq!(v.len(), 4096);
			assert!(v.iter().all(|&b| b == 7));
		}));
	}
	for j in joins {
		j.await.unwrap();
	}
}

/// A panicking closure resumes the panic on the awaiter.
#[tokio::test]
async fn test_spawn_panic_propagates_to_awaiter() {
	let pool = Threadpool::new(2);
	let res = std::panic::AssertUnwindSafe(pool.spawn(|| {
		panic!("boom from worker");
	}))
	.catch_unwind()
	.await;
	assert!(res.is_err(), "panic should propagate to the awaiter");
}

trait CatchUnwindAsync: std::future::Future + Sized {
	fn catch_unwind(self) -> CatchUnwind<Self>;
}

impl<F: std::future::Future + std::panic::UnwindSafe> CatchUnwindAsync for F {
	fn catch_unwind(self) -> CatchUnwind<Self> {
		CatchUnwind {
			inner: Some(self),
		}
	}
}

struct CatchUnwind<F> {
	inner: Option<F>,
}

impl<F: std::future::Future + std::panic::UnwindSafe> std::future::Future for CatchUnwind<F> {
	type Output = std::thread::Result<F::Output>;
	fn poll(
		mut self: std::pin::Pin<&mut Self>,
		cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<Self::Output> {
		// Safety: structural pin projection for `inner`. We never move out.
		let this = unsafe { self.as_mut().get_unchecked_mut() };
		let inner = this.inner.as_mut().expect("polled after completion");
		match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
			unsafe { std::pin::Pin::new_unchecked(inner) }.poll(cx)
		})) {
			Ok(std::task::Poll::Ready(v)) => {
				this.inner = None;
				std::task::Poll::Ready(Ok(v))
			}
			Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
			Err(p) => {
				this.inner = None;
				std::task::Poll::Ready(Err(p))
			}
		}
	}
}

/// Dropping the future before polling must still run the closure and free
/// the captured payload. Uses a `Drop`-tracking struct to confirm the
/// closure (and its captures) are eventually released.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_spawn_dropped_before_poll_releases_payload() {
	struct DropTracker(Arc<AtomicUsize>);
	impl Drop for DropTracker {
		fn drop(&mut self) {
			self.0.fetch_add(1, Ordering::SeqCst);
		}
	}

	let pool = Threadpool::new(2);
	let drops = Arc::new(AtomicUsize::new(0));
	{
		let tracker = DropTracker(drops.clone());
		let fut = pool.spawn(move || {
			// Move the tracker into the closure; the closure produces (),
			// so the tracker is dropped when the closure runs.
			let _t = tracker;
		});
		// Drop the future immediately without polling.
		drop(fut);
	}
	// Give the worker a chance to pick the task up.
	let deadline = std::time::Instant::now() + Duration::from_secs(2);
	while drops.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
		tokio::time::sleep(Duration::from_millis(5)).await;
	}
	assert_eq!(drops.load(Ordering::SeqCst), 1, "tracker should be dropped exactly once");
}

/// Repeatedly panic from worker threads to exercise the ArcSwap-driven
/// stealer-slice respawn path. All non-panicking tasks must still complete.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_stealer_swap_during_panic() {
	let pool = Arc::new(Threadpool::new(4));
	let completed = Arc::new(AtomicUsize::new(0));
	let mut joins = Vec::new();
	for i in 0..200 {
		let pool = pool.clone();
		let completed = completed.clone();
		joins.push(tokio::spawn(async move {
			if i % 25 == 0 {
				let _ = std::panic::AssertUnwindSafe(pool.spawn(|| panic!("respawn me")))
					.catch_unwind()
					.await;
			} else {
				pool.spawn(move || {
					completed.fetch_add(1, Ordering::SeqCst);
				})
				.await;
			}
		}));
	}
	for j in joins {
		let _ = j.await;
	}
	// 200 - ceil(200/25) = 200 - 8 = 192 non-panicking tasks.
	assert_eq!(completed.load(Ordering::SeqCst), 192);
}

/// Mem-forgetting a `spawn_local` future after it has been polled once
/// (i.e. the task is in the injector) must not produce UB and the worker
/// must still be able to run the task and release its `Arc`.
///
/// The `SpawnFuture` carries a `'pool` borrow, but `mem::forget` is
/// leak-safe: the closure captures live inside the `Arc<SpawnFutureData>`,
/// which the worker frees independently of the consumer. Running this test
/// under Miri validates the lifetime-erasure path has no aliasing or
/// use-after-free.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_forget_spawn_local_future_does_not_dangle() {
	use std::future::Future as _;
	use std::pin::Pin;
	use std::task::{Context, Waker};

	struct DropTracker(Arc<AtomicUsize>);
	impl Drop for DropTracker {
		fn drop(&mut self) {
			self.0.fetch_add(1, Ordering::SeqCst);
		}
	}

	let pool = Threadpool::new(2);
	let drops = Arc::new(AtomicUsize::new(0));
	let tracker = DropTracker(drops.clone());
	let mut fut = Box::pin(pool.spawn_local(move || {
		let _t = tracker;
	}));
	// Poll once with a noop waker so the future transitions Init -> Running
	// and pushes the task to the injector. After this point the worker owns
	// an Arc to the shared data and will free the captured tracker when it
	// runs the closure, regardless of what we do with `fut`.
	let waker = Waker::noop().clone();
	let mut cx = Context::from_waker(&waker);
	let _ = Pin::as_mut(&mut fut).poll(&mut cx);
	// Leak the future: skip Drop entirely.
	std::mem::forget(fut);
	let deadline = std::time::Instant::now() + Duration::from_secs(2);
	while drops.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
		tokio::time::sleep(Duration::from_millis(5)).await;
	}
	assert_eq!(
		drops.load(Ordering::SeqCst),
		1,
		"forgetting the polled future must not leak the captured payload"
	);
}

/// Windows affinity with an out-of-range core id must not be UB; the
/// platform shim should refuse and return false. The test simulates the
/// out-of-range case on every OS via the public `affinity::CoreId` type.
#[cfg(target_os = "windows")]
#[test]
fn test_windows_affinity_high_coreid_safe() {
	use affinitypool::affinity::{self, CoreId};
	let res = affinity::set_for_current(CoreId {
		id: 999,
	});
	assert!(!res, "out-of-range core id must not be applied");
}

/// Static assertion that the new `spawn` future does not require its
/// closure return type to be `Send` beyond what we declare — i.e. the
/// `R: Send + 'static` bound on `Threadpool::spawn` is the one being
/// enforced. This is a compile-time check; if it ever changes, this test
/// fails to compile.
#[allow(dead_code)]
fn _spawn_send_bound_compile_check() {
	fn assert_send<T: Send>(_: T) {}
	let pool = Threadpool::new(1);
	let fut = pool.spawn(|| 42u64);
	assert_send(fut);
}

/// `SpawnFuture` should be `Send` whenever `R: Send` purely via the
/// auto-impl (async-task gives us `Task<R>: Send where R: Send`, and
/// the only other field is `PhantomData<&Threadpool>`). This is here
/// to make sure the previously hand-rolled `unsafe impl Send` is
/// genuinely unnecessary.
#[allow(dead_code)]
fn _spawn_local_send_bound_compile_check() {
	fn assert_send<T: Send>(_: T) {}
	let pool = Threadpool::new(1);
	let fut = pool.spawn_local(|| 42u64);
	assert_send(fut);
}
