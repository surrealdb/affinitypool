//! Head-to-head: `affinitypool::Threadpool::spawn` vs
//! [`rayon::ThreadPool::spawn`](https://docs.rs/rayon).
//!
//! Note on fairness: rayon's API is `spawn(closure) -> ()` (fire and
//! forget), built for work-stealing parallelism rather than async
//! producer→worker handoff. To make the comparison apples-to-apples
//! with `Threadpool::spawn(closure).await`, each rayon spawn is wrapped
//! in a `tokio::sync::oneshot` so the producer can await the result.
//! That handshake is the cost of using rayon as a "blocking pool";
//! it's part of what's being measured.
//!
//! Workloads mirror `vs_tokio`: `spawn_overhead`, `round_trip`,
//! `multi_producer`.

use affinitypool::Threadpool;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use rayon::ThreadPoolBuilder;
use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::runtime::Builder as TokioBuilder;
use tokio::sync::oneshot;

fn affinitypool_runtime() -> tokio::runtime::Runtime {
	TokioBuilder::new_current_thread().enable_all().build().unwrap()
}

fn rayon_pool(workers: usize) -> Arc<rayon::ThreadPool> {
	Arc::new(ThreadPoolBuilder::new().num_threads(workers).build().unwrap())
}

fn bench_spawn_overhead(c: &mut Criterion) {
	let mut group = c.benchmark_group("vs_rayon_spawn_overhead");
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

			// rayon
			group.bench_with_input(
				BenchmarkId::new(format!("rayon_{}w", workers), count),
				&(workers, count),
				|b, &(workers, count)| {
					let rt = affinitypool_runtime();
					let pool = rayon_pool(workers);
					b.iter_custom(|iters| {
						let pool = pool.clone();
						rt.block_on(async move {
							let mut total = Duration::ZERO;
							for _ in 0..iters {
								let start = Instant::now();
								let mut handles = Vec::with_capacity(count);
								for i in 0..count {
									let (tx, rx) = oneshot::channel();
									pool.spawn(move || {
										let _ = tx.send(black_box(i));
									});
									handles.push(rx);
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
	let mut group = c.benchmark_group("vs_rayon_round_trip");
	group.measurement_time(Duration::from_secs(10));

	for &workers in &[1usize, 4, 8] {
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

		group.bench_with_input(BenchmarkId::new("rayon", workers), &workers, |b, &workers| {
			let rt = affinitypool_runtime();
			let pool = rayon_pool(workers);
			b.iter_custom(|iters| {
				let pool = pool.clone();
				rt.block_on(async move {
					let start = Instant::now();
					for i in 0..iters {
						let (tx, rx) = oneshot::channel();
						pool.spawn(move || {
							let _ = tx.send(black_box(i));
						});
						black_box(rx.await.unwrap());
					}
					start.elapsed()
				})
			});
		});
	}
	group.finish();
}

fn bench_multi_producer(c: &mut Criterion) {
	let mut group = c.benchmark_group("vs_rayon_multi_producer");
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
							let pool = Arc::new(Threadpool::new(workers));
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

			// rayon
			group.bench_with_input(
				BenchmarkId::new(format!("rayon_{}p_{}w", producers, workers), total_tasks),
				&(producers, workers),
				|b, &(producers, workers)| {
					let rt = TokioBuilder::new_multi_thread()
						.worker_threads(producers)
						.enable_all()
						.build()
						.unwrap();
					let pool = rayon_pool(workers);
					b.iter_custom(|iters| {
						let pool = pool.clone();
						rt.block_on(async move {
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
											let (tx, rx) = oneshot::channel();
											pool.spawn(move || {
												let _ = tx.send(black_box(i));
											});
											hs.push(rx);
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
