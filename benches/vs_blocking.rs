//! Head-to-head: `affinitypool::Threadpool::spawn` vs
//! [`blocking::unblock`](https://docs.rs/blocking) — the pool that
//! powers `async-std::task::spawn_blocking` and is commonly reached
//! for by smol-ecosystem users.
//!
//! Caveat: the `blocking` crate uses a single auto-scaling global
//! pool. The cap is set once per process via the `BLOCKING_MAX_THREADS`
//! environment variable, read lazily on first use. Criterion runs all
//! benches in one process, so we set the cap once at startup and the
//! comparison treats `blocking`'s side as "whatever the pool decides
//! to scale to under load." The affinitypool side still varies its
//! worker count.
//!
//! Same three workloads as `vs_tokio`: `spawn_overhead`, `round_trip`,
//! `multi_producer`.

use affinitypool::Threadpool;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use std::time::{Duration, Instant};
use tokio::runtime::Builder as TokioBuilder;

fn affinitypool_runtime() -> tokio::runtime::Runtime {
	TokioBuilder::new_current_thread().enable_all().build().unwrap()
}

fn bench_spawn_overhead(c: &mut Criterion) {
	let mut group = c.benchmark_group("vs_blocking_spawn_overhead");
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
		}

		// blocking::unblock (pool size is global / auto-scaled)
		group.bench_with_input(
			BenchmarkId::new("blocking", count),
			&count,
			|b, &count| {
				let rt = affinitypool_runtime();
				b.iter_custom(|iters| {
					rt.block_on(async move {
						let _ = blocking::unblock(|| 0u32).await;
						let mut total = Duration::ZERO;
						for _ in 0..iters {
							let start = Instant::now();
							let mut handles = Vec::with_capacity(count);
							for i in 0..count {
								handles.push(blocking::unblock(move || black_box(i)));
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
	}
	group.finish();
}

fn bench_round_trip(c: &mut Criterion) {
	let mut group = c.benchmark_group("vs_blocking_round_trip");
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
	}

	group.bench_function("blocking", |b| {
		let rt = affinitypool_runtime();
		b.iter_custom(|iters| {
			rt.block_on(async move {
				let _ = blocking::unblock(|| 0u32).await;
				let start = Instant::now();
				for i in 0..iters {
					black_box(blocking::unblock(move || black_box(i)).await);
				}
				start.elapsed()
			})
		});
	});
	group.finish();
}

fn bench_multi_producer(c: &mut Criterion) {
	let mut group = c.benchmark_group("vs_blocking_multi_producer");
	group.measurement_time(Duration::from_secs(10));

	for &producers in &[2usize, 4, 8] {
		let total_tasks = producers * 1_000;
		group.throughput(Throughput::Elements(total_tasks as u64));

		for &workers in &[1usize, 4] {
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
		}

		// blocking
		group.bench_with_input(
			BenchmarkId::new(format!("blocking_{}p", producers), total_tasks),
			&producers,
			|b, &producers| {
				let rt = TokioBuilder::new_multi_thread()
					.worker_threads(producers)
					.enable_all()
					.build()
					.unwrap();
				b.iter_custom(|iters| {
					rt.block_on(async move {
						let _ = blocking::unblock(|| 0u32).await;
						let per = 1_000usize;
						let mut total = Duration::ZERO;
						for _ in 0..iters {
							let start = Instant::now();
							let mut producer_handles = Vec::with_capacity(producers);
							for _ in 0..producers {
								producer_handles.push(tokio::spawn(async move {
									let mut hs = Vec::with_capacity(per);
									for i in 0..per {
										hs.push(blocking::unblock(move || black_box(i)));
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
	}
	group.finish();
}

criterion_group!(benches, bench_spawn_overhead, bench_round_trip, bench_multi_producer);
criterion_main!(benches);
