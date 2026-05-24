//! Per-spawn allocation count probe.
//!
//! Wraps the system allocator in a counter and reports the number of
//! allocations and deallocations charged to a `Threadpool::spawn` round
//! trip (submit + await), and to a `Threadpool::spawn_local` round trip.
//!
//! Run with:
//!
//! ```sh
//! cargo run --release --example alloc_count
//! ```
//!
//! The output is the *delta* observed by wrapping the spawn/await in
//! before/after snapshots of the counting allocator. This is the cleanest
//! way to measure the per-task allocator pressure that the performance
//! work targets (changes #1 fused-allocation and #2 inline storage).
//!
//! Note: the counter sees *every* allocation the program makes, including
//! tokio's runtime allocations. The probe takes care to do nothing other
//! than the spawn/await within the measured region, but the count is best
//! treated as a relative baseline for comparing branches, not an absolute
//! lower bound.

use affinitypool::Threadpool;
use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static DEALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES_ALLOCATED: AtomicUsize = AtomicUsize::new(0);

struct CountingAlloc;

unsafe impl GlobalAlloc for CountingAlloc {
	unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
		ALLOCS.fetch_add(1, Ordering::Relaxed);
		BYTES_ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
		unsafe { System.alloc(layout) }
	}

	unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
		DEALLOCS.fetch_add(1, Ordering::Relaxed);
		unsafe { System.dealloc(ptr, layout) }
	}
}

#[global_allocator]
static ALLOCATOR: CountingAlloc = CountingAlloc;

#[derive(Copy, Clone)]
struct Snapshot {
	allocs: usize,
	deallocs: usize,
	bytes: usize,
}

impl Snapshot {
	fn take() -> Self {
		Self {
			allocs: ALLOCS.load(Ordering::Relaxed),
			deallocs: DEALLOCS.load(Ordering::Relaxed),
			bytes: BYTES_ALLOCATED.load(Ordering::Relaxed),
		}
	}

	fn delta(self, prior: Snapshot) -> (usize, usize, usize) {
		(self.allocs - prior.allocs, self.deallocs - prior.deallocs, self.bytes - prior.bytes)
	}
}

fn main() {
	let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();

	rt.block_on(async {
		let pool = Threadpool::new(2);
		// Warm up: spawn-up of the workers themselves and any first-touch
		// lazy allocations should not be charged to the per-spawn delta.
		for _ in 0..16 {
			pool.spawn(|| ()).await;
			pool.spawn_local(|| ()).await;
		}

		report_spawn(&pool, "spawn(empty closure)", 1_000).await;
		report_spawn(&pool, "spawn(empty closure)", 10_000).await;
		report_spawn_local(&pool, "spawn_local(empty closure)", 1_000).await;
		report_spawn_local(&pool, "spawn_local(empty closure)", 10_000).await;
	});
}

async fn report_spawn(pool: &Threadpool, label: &str, n: usize) {
	let before = Snapshot::take();
	for _ in 0..n {
		let v = pool.spawn(|| black_box(0u32)).await;
		black_box(v);
	}
	let after = Snapshot::take();
	let (a, d, b) = after.delta(before);
	println!(
		"{label:40} n={n:>6}  allocs={a:>7}  deallocs={d:>7}  bytes={b:>9}  alloc/task={alloc_per:>5.2}  byte/task={byte_per:>6.1}",
		alloc_per = a as f64 / n as f64,
		byte_per = b as f64 / n as f64,
	);
}

async fn report_spawn_local(pool: &Threadpool, label: &str, n: usize) {
	let before = Snapshot::take();
	for _ in 0..n {
		let v = pool.spawn_local(|| black_box(0u32)).await;
		black_box(v);
	}
	let after = Snapshot::take();
	let (a, d, b) = after.delta(before);
	println!(
		"{label:40} n={n:>6}  allocs={a:>7}  deallocs={d:>7}  bytes={b:>9}  alloc/task={alloc_per:>5.2}  byte/task={byte_per:>6.1}",
		alloc_per = a as f64 / n as f64,
		byte_per = b as f64 / n as f64,
	);
}
