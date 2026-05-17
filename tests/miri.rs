//! Tests targeted at unsafe code paths. These are written so that
//! `cargo +nightly miri test --test miri` exercises:
//!
//! - `OwnedTask` construction, execution, and drop-without-run for several
//!   closure shapes (ZST, large captures, types with non-trivial `Drop`).
//! - The lifetime-erasure path used by `spawn_local`.
//! - Concurrent submission to a small pool from multiple threads.
//!
//! Miri-gated `#[cfg(miri)]` scaling keeps counts low enough to run in a
//! few minutes; the same tests run unmodified on stable as a sanity check.

#![cfg(not(loom))]

use affinitypool::Threadpool;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(miri)]
const TASKS: usize = 16;
#[cfg(not(miri))]
const TASKS: usize = 256;

/// `OwnedTask::new` + `run` for several closure types. Exercises the
/// vtable cast in `src/task.rs`. Miri catches type-mismatched calls or
/// double-frees here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn miri_owned_task_shapes() {
	let pool = Threadpool::new(2);
	// ZST return.
	pool.spawn(|| {}).await;
	// Small capture, primitive return.
	let n = pool.spawn(|| 1234u64 ^ 0xdeadbeef).await;
	assert_eq!(n, 1234u64 ^ 0xdeadbeef);
	// Large capture (heap allocation moved into the closure).
	let big = vec![0u8; 8192];
	let len = pool.spawn(move || big.len()).await;
	assert_eq!(len, 8192);
	// Capture with non-trivial Drop.
	struct DropMe(Arc<AtomicUsize>);
	impl Drop for DropMe {
		fn drop(&mut self) {
			self.0.fetch_add(1, Ordering::SeqCst);
		}
	}
	let drops = Arc::new(AtomicUsize::new(0));
	let d = DropMe(drops.clone());
	pool.spawn(move || {
		let _d = d;
	})
	.await;
	assert_eq!(drops.load(Ordering::SeqCst), 1);
}

/// Concurrent submission from multiple Tokio tasks onto a 2-worker pool.
/// Exercises the `ArcSwap` stealer slice and the parking/unparking logic
/// under contention.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn miri_concurrent_spawn() {
	let pool = Arc::new(Threadpool::new(2));
	let total = Arc::new(AtomicUsize::new(0));
	let mut joins = Vec::new();
	for i in 0..TASKS {
		let pool = pool.clone();
		let total = total.clone();
		joins.push(tokio::spawn(async move {
			let v = pool.spawn(move || i).await;
			total.fetch_add(v, Ordering::SeqCst);
		}));
	}
	for j in joins {
		j.await.unwrap();
	}
	let expected: usize = (0..TASKS).sum();
	assert_eq!(total.load(Ordering::SeqCst), expected);
}

/// `spawn_local` lifetime-erasure path: capture a stack borrow, let the
/// future drive to completion. Miri checks that the worker's access to the
/// borrowed data is within the borrow's lifetime.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn miri_spawn_local_borrow() {
	let pool = Threadpool::new(2);
	let data: Vec<u32> = (0..64).collect();
	let sum = pool.spawn_local(|| data.iter().sum::<u32>()).await;
	let expected: u32 = (0..64).sum();
	assert_eq!(sum, expected);
}

/// Drop a `spawn` future before it resolves. Miri checks that the worker's
/// late write into the shared `SpawnCompletion` slot doesn't race with the
/// awaiter's `Drop` impl, and that the payload is freed exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn miri_drop_future_before_complete() {
	struct DropMe(Arc<AtomicUsize>);
	impl Drop for DropMe {
		fn drop(&mut self) {
			self.0.fetch_add(1, Ordering::SeqCst);
		}
	}
	let pool = Threadpool::new(2);
	let drops = Arc::new(AtomicUsize::new(0));
	{
		let d = DropMe(drops.clone());
		let fut = pool.spawn(move || {
			// Return the captured tracker so the SpawnCompletion slot owns
			// it; if the awaiter is dropped before reading, the
			// SpawnCompletion's own Drop impl must free it exactly once.
			d
		});
		drop(fut);
	}
	// Give the worker time to run on non-Miri targets; under Miri the test
	// will still rely on the pool's Drop to join workers.
	#[cfg(not(miri))]
	tokio::time::sleep(std::time::Duration::from_millis(50)).await;
	drop(pool); // joins all workers
	assert_eq!(drops.load(Ordering::SeqCst), 1);
}
