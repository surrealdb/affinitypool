//! Head-to-head: `affinitypool::Threadpool::spawn` vs `tokio::task::spawn_blocking`.
//!
//! The same async producer drives both. For affinitypool, the
//! Tokio runtime is a single-threaded `current_thread` (just the
//! producer) and the worker side is `N` affinitypool workers.
//! For tokio's blocking pool, the same `current_thread` runtime
//! is built with `max_blocking_threads(N)` so the worker side is
//! `N` threads from tokio's blocking pool.
//!
//! Workloads:
//!
//! * **`spawn_overhead`** — submit `count` empty closures and await
//!   each. Dominated by per-spawn allocator + handshake cost.
//! * **`round_trip`** — submit one task, await it, repeat. Measures
//!   producer↔worker latency including the wakeup path.
//! * **`multi_producer`** — `P` concurrent async producers each
//!   feeding `count / P` tasks into one pool. Stresses contention
//!   on whatever queue the pool uses internally.

use affinitypool::Threadpool;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
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

fn bench_spawn_overhead(c: &mut Criterion) {
	let mut group = c.benchmark_group("vs_tokio_spawn_overhead");
	group.measurement_time(Duration::from_secs(10));

	for &count in &[1usize, 100, 1_000, 10_000] {
		group.throughput(Throughput::Elements(count as u64));
		for &workers in &[1usize, 4] {
			// affinitypool
			group.bench_with_input(
				BenchmarkId::new(format!("affinitypool_{}w", workers), count),
				&(workers, count),
				|b, &(workers, count)| {
					let rt = affinitypool_runtime();
					b.iter_custom(|iters| {
						rt.block_on(async move {
							let pool = Threadpool::new(workers);
							// Warm-up
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
				},
			);

			// tokio::spawn_blocking
			group.bench_with_input(
				BenchmarkId::new(format!("tokio_{}w", workers), count),
				&(workers, count),
				|b, &(workers, count)| {
					let rt = tokio_runtime(workers);
					b.iter_custom(|iters| {
						rt.block_on(async move {
							// Warm-up
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
				},
			);
		}
	}
	group.finish();
}

fn bench_round_trip(c: &mut Criterion) {
	let mut group = c.benchmark_group("vs_tokio_round_trip");
	group.measurement_time(Duration::from_secs(10));

	for &workers in &[1usize, 4, 8] {
		// affinitypool
		group.bench_with_input(
			BenchmarkId::new("affinitypool", workers),
			&workers,
			|b, &workers| {
				let rt = affinitypool_runtime();
				b.iter_custom(|iters| {
					rt.block_on(async move {
						let pool = Threadpool::new(workers);
						let _ = pool.spawn(|| 0u32).await;
						let start = Instant::now();
						for i in 0..iters {
							black_box(pool.spawn(move || black_box(i)).await);
						}
						start.elapsed()
					})
				});
			},
		);

		// tokio
		group.bench_with_input(BenchmarkId::new("tokio", workers), &workers, |b, &workers| {
			let rt = tokio_runtime(workers);
			b.iter_custom(|iters| {
				rt.block_on(async move {
					let _ = tokio::task::spawn_blocking(|| 0u32).await;
					let start = Instant::now();
					for i in 0..iters {
						black_box(tokio::task::spawn_blocking(move || black_box(i)).await.unwrap());
					}
					start.elapsed()
				})
			});
		});
	}
	group.finish();
}

fn bench_multi_producer(c: &mut Criterion) {
	let mut group = c.benchmark_group("vs_tokio_multi_producer");
	group.measurement_time(Duration::from_secs(10));

	for &producers in &[2usize, 4, 8] {
		for &workers in &[1usize, 4] {
			let total_tasks = producers * 1_000;
			group.throughput(Throughput::Elements(total_tasks as u64));

			// affinitypool
			group.bench_with_input(
				BenchmarkId::new(format!("affinitypool_{}p_{}w", producers, workers), total_tasks),
				&(producers, workers),
				|b, &(producers, workers)| {
					let rt = TokioBuilder::new_multi_thread()
						.worker_threads(producers)
						.enable_all()
						.build()
						.unwrap();
					b.iter_custom(|iters| {
						rt.block_on(async move {
							let pool = std::sync::Arc::new(Threadpool::new(workers));
							let _ = pool.spawn(|| 0u32).await;
							let per = 1_000usize;
							let mut total = Duration::ZERO;
							for _ in 0..iters {
								let start = Instant::now();
								let mut producer_handles = Vec::with_capacity(producers);
								for _ in 0..producers {
									let pool = pool.clone();
									producer_handles.push(tokio::spawn(async move {
										let mut hs = Vec::with_capacity(per);
										for i in 0..per {
											hs.push(pool.spawn(move || black_box(i)));
										}
										for h in hs {
											black_box(h.await);
										}
									}));
								}
								for ph in producer_handles {
									ph.await.unwrap();
								}
								total += start.elapsed();
							}
							total
						})
					});
				},
			);

			// tokio
			group.bench_with_input(
				BenchmarkId::new(format!("tokio_{}p_{}w", producers, workers), total_tasks),
				&(producers, workers),
				|b, &(producers, workers)| {
					let rt = TokioBuilder::new_multi_thread()
						.worker_threads(producers)
						.max_blocking_threads(workers)
						.enable_all()
						.build()
						.unwrap();
					b.iter_custom(|iters| {
						rt.block_on(async move {
							let _ = tokio::task::spawn_blocking(|| 0u32).await;
							let per = 1_000usize;
							let mut total = Duration::ZERO;
							for _ in 0..iters {
								let start = Instant::now();
								let mut producer_handles = Vec::with_capacity(producers);
								for _ in 0..producers {
									producer_handles.push(tokio::spawn(async move {
										let mut hs = Vec::with_capacity(per);
										for i in 0..per {
											hs.push(tokio::task::spawn_blocking(move || {
												black_box(i)
											}));
										}
										for h in hs {
											black_box(h.await.unwrap());
										}
									}));
								}
								for ph in producer_handles {
									ph.await.unwrap();
								}
								total += start.elapsed();
							}
							total
						})
					});
				},
			);
		}
	}
	group.finish();
}

criterion_group!(benches, bench_spawn_overhead, bench_round_trip, bench_multi_producer);
criterion_main!(benches);
