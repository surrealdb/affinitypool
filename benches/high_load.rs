//! High-load benchmarks targeting workloads the `vs_tokio.rs` suite
//! doesn't exercise:
//!
//! * **`burst_drain`** — push a large burst (100k / 1M tasks) in one
//!   go and measure time to drain. Stresses peak queue depth,
//!   allocator pressure, and the worst-case mutex contention window.
//! * **`sustained_throughput`** — repeat 100k-task spawn/await
//!   batches for criterion's measurement window. Tests whether peak
//!   burst rates hold under continuous load (no slow leaks, no
//!   degradation, no allocator pathologies).
//! * **`realistic_cost`** — sweep the per-task CPU cost from ~50 ns
//!   (empty closure) through ~100 µs (10⁵ arithmetic ops). Shows how
//!   pool overhead amortises as tasks get larger and where contention
//!   stops mattering.
//!
//! Each benchmark has an `affinitypool` and `tokio_spawn_blocking`
//! variant matched in worker count, so the criterion HTML report
//! places them side-by-side under the same group.

use affinitypool::Threadpool;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::runtime::Builder as TokioBuilder;

fn affinitypool_runtime() -> tokio::runtime::Runtime {
	TokioBuilder::new_current_thread().enable_all().build().unwrap()
}

fn tokio_runtime(blocking_threads: usize) -> tokio::runtime::Runtime {
	TokioBuilder::new_current_thread()
		.enable_all()
		.max_blocking_threads(blocking_threads)
		.build()
		.unwrap()
}

/// Deterministic CPU work. `iters` controls cost.
///
/// The inner loop uses `black_box` on every operand so the optimiser
/// can't vectorise or unroll the body — without this, even
/// `100_000`-iteration variants were finishing in microseconds,
/// indicating the loop was being collapsed at compile time.
///
/// | `iters`   | wall cost (approx, x86_64, release) |
/// |-----------|-------------------------------------|
/// | 0         | ~5 ns (function call only)          |
/// | 100       | ~250 ns                             |
/// | 1_000     | ~2.5 µs                             |
/// | 10_000    | ~25 µs                              |
/// | 100_000   | ~250 µs                             |
#[inline(never)]
fn cpu_task(iters: usize) -> usize {
	let mut acc: usize = black_box(0);
	for i in 0..iters {
		// `black_box` on both operands prevents loop unrolling and
		// strength reduction; the compiler must treat each iteration
		// as opaque.
		acc = black_box(acc).wrapping_add(black_box(i).wrapping_mul(17));
	}
	black_box(acc)
}

// ───────────────────────────────────────────────────────────────────
// burst_drain
// ───────────────────────────────────────────────────────────────────

fn bench_burst_drain(c: &mut Criterion) {
	let mut group = c.benchmark_group("burst_drain");
	// One iter at 1M tasks runs in roughly a second; keep the suite
	// reasonable by capping sample size.
	group.sample_size(10);
	group.measurement_time(Duration::from_secs(15));

	for &count in &[100_000usize, 1_000_000] {
		group.throughput(Throughput::Elements(count as u64));
		let workers = 4usize;

		group.bench_with_input(BenchmarkId::new("affinitypool", count), &count, |b, &count| {
			let rt = affinitypool_runtime();
			b.iter_custom(|iters| {
				rt.block_on(async move {
					let pool = Threadpool::new(workers);
					let _ = pool.spawn(|| 0u32).await;
					let mut total = Duration::ZERO;
					for _ in 0..iters {
						let start = Instant::now();
						let mut handles = Vec::with_capacity(count);
						for i in 0..count {
							handles.push(pool.spawn(move || black_box(i)));
						}
						for h in handles {
							black_box(h.await);
						}
						total += start.elapsed();
					}
					total
				})
			});
		});

		group.bench_with_input(BenchmarkId::new("tokio", count), &count, |b, &count| {
			let rt = tokio_runtime(workers);
			b.iter_custom(|iters| {
				rt.block_on(async move {
					let _ = tokio::task::spawn_blocking(|| 0u32).await;
					let mut total = Duration::ZERO;
					for _ in 0..iters {
						let start = Instant::now();
						let mut handles = Vec::with_capacity(count);
						for i in 0..count {
							handles.push(tokio::task::spawn_blocking(move || black_box(i)));
						}
						for h in handles {
							black_box(h.await.unwrap());
						}
						total += start.elapsed();
					}
					total
				})
			});
		});
	}
	group.finish();
}

// ───────────────────────────────────────────────────────────────────
// sustained_throughput
// ───────────────────────────────────────────────────────────────────

/// Repeatedly push and drain 100k-task batches for the duration of
/// the criterion measurement window. Each criterion iter is one
/// batch; criterion runs many iters back-to-back through the same
/// pool, which is the closest off-the-shelf criterion can get to
/// "sustained N-second load".
fn bench_sustained_throughput(c: &mut Criterion) {
	let mut group = c.benchmark_group("sustained_throughput");
	const BATCH: usize = 100_000;
	group.throughput(Throughput::Elements(BATCH as u64));
	// Long measurement window so we see steady-state behaviour, not
	// just the first warm-up pass.
	group.measurement_time(Duration::from_secs(20));
	group.sample_size(20);

	let workers = 4usize;

	group.bench_function("affinitypool", |b| {
		let rt = affinitypool_runtime();
		b.iter_custom(|iters| {
			rt.block_on(async move {
				let pool = Threadpool::new(workers);
				let _ = pool.spawn(|| 0u32).await;
				let mut total = Duration::ZERO;
				for _ in 0..iters {
					let start = Instant::now();
					let mut handles = Vec::with_capacity(BATCH);
					for i in 0..BATCH {
						handles.push(pool.spawn(move || black_box(i)));
					}
					for h in handles {
						black_box(h.await);
					}
					total += start.elapsed();
				}
				total
			})
		});
	});

	group.bench_function("tokio", |b| {
		let rt = tokio_runtime(workers);
		b.iter_custom(|iters| {
			rt.block_on(async move {
				let _ = tokio::task::spawn_blocking(|| 0u32).await;
				let mut total = Duration::ZERO;
				for _ in 0..iters {
					let start = Instant::now();
					let mut handles = Vec::with_capacity(BATCH);
					for i in 0..BATCH {
						handles.push(tokio::task::spawn_blocking(move || black_box(i)));
					}
					for h in handles {
						black_box(h.await.unwrap());
					}
					total += start.elapsed();
				}
				total
			})
		});
	});

	group.finish();
}

// ───────────────────────────────────────────────────────────────────
// realistic_cost
// ───────────────────────────────────────────────────────────────────

/// Sweep per-task CPU cost. As tasks grow larger relative to pool
/// overhead, contention on the shared queue should stop mattering;
/// the two implementations should converge. Crossover usually lands
/// somewhere around per-task cost ≈ 10 µs on a typical x86_64 box.
fn bench_realistic_cost(c: &mut Criterion) {
	let mut group = c.benchmark_group("realistic_cost");
	// 10k tasks * 100k iters/task = 10⁹ adds at the high end. Bigger
	// measurement window lets criterion get stable samples without
	// blowing up wall time too much; sample size kept modest because
	// each iter at the high end already takes hundreds of ms.
	group.measurement_time(Duration::from_secs(15));
	group.sample_size(15);
	let count = 10_000usize;
	let workers = 4usize;
	group.throughput(Throughput::Elements(count as u64));

	for &cost in &[0usize, 100, 1_000, 10_000, 100_000] {
		group.bench_with_input(BenchmarkId::new("affinitypool", cost), &cost, |b, &cost| {
			let rt = affinitypool_runtime();
			b.iter_custom(|iters| {
				rt.block_on(async move {
					let pool = Threadpool::new(workers);
					let _ = pool.spawn(move || cpu_task(cost)).await;
					let mut total = Duration::ZERO;
					for _ in 0..iters {
						let start = Instant::now();
						let mut handles = Vec::with_capacity(count);
						for _ in 0..count {
							handles.push(pool.spawn(move || cpu_task(cost)));
						}
						for h in handles {
							black_box(h.await);
						}
						total += start.elapsed();
					}
					total
				})
			});
		});

		group.bench_with_input(BenchmarkId::new("tokio", cost), &cost, |b, &cost| {
			let rt = tokio_runtime(workers);
			b.iter_custom(|iters| {
				rt.block_on(async move {
					let _ = tokio::task::spawn_blocking(move || cpu_task(cost)).await;
					let mut total = Duration::ZERO;
					for _ in 0..iters {
						let start = Instant::now();
						let mut handles = Vec::with_capacity(count);
						for _ in 0..count {
							handles.push(tokio::task::spawn_blocking(move || cpu_task(cost)));
						}
						for h in handles {
							black_box(h.await.unwrap());
						}
						total += start.elapsed();
					}
					total
				})
			});
		});
	}
	group.finish();
}

// ───────────────────────────────────────────────────────────────────
// concurrent_pipeline (multi-producer sustained load)
// ───────────────────────────────────────────────────────────────────

/// 8 concurrent async producers feeding one pool, each pushing 100k
/// tasks. Closer to a real server workload than the single-producer
/// patterns above. The total of 800k in-flight tasks also exercises
/// queue depth growth.
fn bench_concurrent_pipeline(c: &mut Criterion) {
	let mut group = c.benchmark_group("concurrent_pipeline");
	const PRODUCERS: usize = 8;
	const PER_PRODUCER: usize = 100_000;
	const TOTAL: usize = PRODUCERS * PER_PRODUCER;
	group.throughput(Throughput::Elements(TOTAL as u64));
	group.measurement_time(Duration::from_secs(20));
	group.sample_size(10);

	let workers = 8usize;

	group.bench_function("affinitypool", |b| {
		let rt = TokioBuilder::new_multi_thread()
			.worker_threads(PRODUCERS)
			.enable_all()
			.build()
			.unwrap();
		b.iter_custom(|iters| {
			rt.block_on(async move {
				let pool = Arc::new(Threadpool::new(workers));
				let _ = pool.spawn(|| 0u32).await;
				let mut total = Duration::ZERO;
				for _ in 0..iters {
					let start = Instant::now();
					let mut producers = Vec::with_capacity(PRODUCERS);
					for _ in 0..PRODUCERS {
						let pool = pool.clone();
						producers.push(tokio::spawn(async move {
							let mut hs = Vec::with_capacity(PER_PRODUCER);
							for i in 0..PER_PRODUCER {
								hs.push(pool.spawn(move || black_box(i)));
							}
							for h in hs {
								black_box(h.await);
							}
						}));
					}
					for p in producers {
						p.await.unwrap();
					}
					total += start.elapsed();
				}
				total
			})
		});
	});

	group.bench_function("tokio", |b| {
		let rt = TokioBuilder::new_multi_thread()
			.worker_threads(PRODUCERS)
			.max_blocking_threads(workers)
			.enable_all()
			.build()
			.unwrap();
		b.iter_custom(|iters| {
			rt.block_on(async move {
				let _ = tokio::task::spawn_blocking(|| 0u32).await;
				let mut total = Duration::ZERO;
				for _ in 0..iters {
					let start = Instant::now();
					let mut producers = Vec::with_capacity(PRODUCERS);
					for _ in 0..PRODUCERS {
						producers.push(tokio::spawn(async move {
							let mut hs = Vec::with_capacity(PER_PRODUCER);
							for i in 0..PER_PRODUCER {
								hs.push(tokio::task::spawn_blocking(move || black_box(i)));
							}
							for h in hs {
								black_box(h.await.unwrap());
							}
						}));
					}
					for p in producers {
						p.await.unwrap();
					}
					total += start.elapsed();
				}
				total
			})
		});
	});

	group.finish();
}

criterion_group!(
	high_load,
	bench_burst_drain,
	bench_sustained_throughput,
	bench_realistic_cost,
	bench_concurrent_pipeline
);
criterion_main!(high_load);
