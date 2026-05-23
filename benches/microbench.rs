//! Microbenchmarks for the per-spawn hot path.
//!
//! These complement `benches/threadpool.rs` (which mixes spawn cost with
//! per-task compute) by isolating the components that the performance work
//! actually targets:
//!
//! * **`spawn_overhead`** — empty closure, single worker. Dominated by
//!   allocator traffic and the spawn/poll state machine.
//! * **`steady_state_busy`** — keep all workers busy with many tasks queued,
//!   so the parking machinery is never exercised. Isolates the SeqCst fences
//!   and the `parked_threads.pop()` cost on the producer hot path.
//! * **`park_unpark_handshake`** — submit a single task, await, then go
//!   idle. Forces a park/unpark cycle on each iteration. Stresses the
//!   handshake the SeqCst fences exist to protect.
//! * **`multi_producer_contention`** — many concurrent async producers fed
//!   into one pool. Stresses contention on the global injector and the
//!   parked-threads queue.
//! * **`spawn_local_overhead`** — empty closure via `spawn_local`. The
//!   `spawn_local` path uses different completion machinery and is not
//!   covered by the existing benches.
//! * **`steal_imbalance`** — push every task through the global injector
//!   with N workers idle. Stresses the work-stealing path that distributes
//!   work to peer workers.

use affinitypool::{Builder, Threadpool};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;

/// Submit `count` empty closures and await each. The closure does no work,
/// so the timing is dominated by spawn/poll overhead.
fn bench_spawn_overhead(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();
	let mut group = c.benchmark_group("spawn_overhead");
	group.measurement_time(Duration::from_secs(10));

	for &count in &[1usize, 100, 1_000, 10_000] {
		group.throughput(Throughput::Elements(count as u64));
		for &workers in &[1usize, 4] {
			group.bench_with_input(
				BenchmarkId::new(format!("{}_workers", workers), count),
				&(workers, count),
				|b, &(workers, count)| {
					b.iter_custom(|iters| {
						rt.block_on(async move {
							let pool = Threadpool::new(workers);
							// Warm up the workers so the first iteration is
							// not biased by the spawn-up cost.
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
	}
	group.finish();
}

/// Keep the pool saturated: queue 8x more tasks than there are workers
/// before any complete, then await all. Workers should never park, so the
/// producer hot path (`spawn` → injector push → opportunistic unpark) and
/// the worker hot path (`find_task` busy loop) are both isolated.
fn bench_steady_state_busy(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();
	let mut group = c.benchmark_group("steady_state_busy");
	group.measurement_time(Duration::from_secs(10));

	const TASK_PER_WORKER: usize = 4096;
	for &workers in &[1usize, 4, 8] {
		let total_tasks = workers * TASK_PER_WORKER;
		group.throughput(Throughput::Elements(total_tasks as u64));
		group.bench_with_input(
			BenchmarkId::new("workers", workers),
			&workers,
			|b, &workers| {
				b.iter_custom(|iters| {
					rt.block_on(async move {
						let pool = Threadpool::new(workers);
						// Warm up.
						let _ = pool.spawn(|| ()).await;
						let mut total = Duration::ZERO;
						for _ in 0..iters {
							let start = Instant::now();
							let mut handles = Vec::with_capacity(total_tasks);
							// Submit all tasks before awaiting any: workers
							// will always find something in the injector
							// or each other's queues and never park.
							for i in 0..total_tasks {
								handles.push(pool.spawn(move || {
									// Tiny non-trivial work so the
									// compiler can't optimise away.
									let mut s = i;
									for _ in 0..8 {
										s = s.wrapping_mul(17).wrapping_add(1);
									}
									black_box(s)
								}));
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

/// One task per iteration, awaited before the next is submitted. The pool
/// goes fully idle between tasks, so each iteration exercises a full
/// park/unpark cycle. This is what the SeqCst fences in `spawn` and the
/// worker loop exist to protect.
fn bench_park_unpark_handshake(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();
	let mut group = c.benchmark_group("park_unpark_handshake");
	group.measurement_time(Duration::from_secs(10));

	for &workers in &[1usize, 4, 8] {
		group.bench_with_input(
			BenchmarkId::new("workers", workers),
			&workers,
			|b, &workers| {
				b.iter_custom(|iters| {
					rt.block_on(async move {
						let pool = Threadpool::new(workers);
						// Warm the workers and let them park.
						let _ = pool.spawn(|| ()).await;
						// A short delay encourages all workers to be
						// parked before the measured iterations begin.
						tokio::time::sleep(Duration::from_millis(2)).await;
						let mut total = Duration::ZERO;
						for _ in 0..iters {
							let start = Instant::now();
							let v = pool.spawn(|| black_box(42u64)).await;
							total += start.elapsed();
							black_box(v);
						}
						total
					})
				});
			},
		);
	}
	group.finish();
}

/// N tokio tasks each submit `tasks_per_producer` jobs into the same pool
/// concurrently. Stresses contention on the global injector and the
/// bounded `parked_threads` queue from the producer side.
fn bench_multi_producer_contention(c: &mut Criterion) {
	let rt = tokio::runtime::Builder::new_multi_thread()
		.worker_threads(8)
		.enable_all()
		.build()
		.unwrap();
	let mut group = c.benchmark_group("multi_producer_contention");
	group.measurement_time(Duration::from_secs(10));

	const TASKS_PER_PRODUCER: usize = 1_000;
	for &producers in &[2usize, 4, 8] {
		for &workers in &[1usize, 4] {
			let total = producers * TASKS_PER_PRODUCER;
			group.throughput(Throughput::Elements(total as u64));
			group.bench_with_input(
				BenchmarkId::new(
					format!("{}_producers_{}_workers", producers, workers),
					total,
				),
				&(producers, workers),
				|b, &(producers, workers)| {
					b.iter_custom(|iters| {
						let rt = &rt;
						rt.block_on(async move {
							let mut total_dur = Duration::ZERO;
							for _ in 0..iters {
								let pool = Arc::new(Threadpool::new(workers));
								let start = Instant::now();
								let mut producer_handles = Vec::with_capacity(producers);
								for _ in 0..producers {
									let pool = pool.clone();
									producer_handles.push(tokio::spawn(async move {
										let mut handles = Vec::with_capacity(TASKS_PER_PRODUCER);
										for i in 0..TASKS_PER_PRODUCER {
											handles.push(pool.spawn(move || black_box(i)));
										}
										for h in handles {
											black_box(h.await);
										}
									}));
								}
								for ph in producer_handles {
									ph.await.unwrap();
								}
								total_dur += start.elapsed();
							}
							total_dur
						})
					});
				},
			);
		}
	}
	group.finish();
}

/// Empty closure via `spawn_local`. Isolates the local completion
/// machinery (Mutex + Condvar + AtomicWaker today) from the spawn path.
fn bench_spawn_local_overhead(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();
	let mut group = c.benchmark_group("spawn_local_overhead");
	group.measurement_time(Duration::from_secs(10));

	for &count in &[1usize, 100, 1_000] {
		group.throughput(Throughput::Elements(count as u64));
		group.bench_with_input(BenchmarkId::new("4_workers", count), &count, |b, &count| {
			b.iter_custom(|iters| {
				rt.block_on(async move {
					let pool = Threadpool::new(4);
					// Warm up.
					pool.spawn_local(|| ()).await;
					let mut total = Duration::ZERO;
					for _ in 0..iters {
						let start = Instant::now();
						// `spawn_local` returns a future that borrows the
						// pool, so we await sequentially rather than
						// collecting handles (the borrow would prevent
						// stacking them in a Vec without boxing).
						for _ in 0..count {
							pool.spawn_local(|| black_box(())).await;
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

/// All workers initially idle and parked; a single producer dumps a large
/// batch into the injector and awaits completion. Stresses the steal path
/// (work-stealing distribution + opportunistic unparks) more than the
/// producer path.
fn bench_steal_imbalance(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();
	let mut group = c.benchmark_group("steal_imbalance");
	group.measurement_time(Duration::from_secs(10));

	const TASKS: usize = 10_000;
	for &workers in &[2usize, 4, 8] {
		group.throughput(Throughput::Elements(TASKS as u64));
		group.bench_with_input(
			BenchmarkId::new(format!("{}_workers", workers), TASKS),
			&workers,
			|b, &workers| {
				b.iter_custom(|iters| {
					rt.block_on(async move {
						let pool = Threadpool::new(workers);
						// Warm up then idle.
						let _ = pool.spawn(|| ()).await;
						tokio::time::sleep(Duration::from_millis(2)).await;
						let mut total = Duration::ZERO;
						for _ in 0..iters {
							let start = Instant::now();
							let mut handles = Vec::with_capacity(TASKS);
							for i in 0..TASKS {
								handles.push(pool.spawn(move || {
									let mut s = i;
									for _ in 0..16 {
										s = s.wrapping_mul(17).wrapping_add(1);
									}
									black_box(s)
								}));
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

/// `thread_per_core` configuration, steady-state busy. Matches the
/// production deployment in `surrealdb` and gives us a fixed reference
/// point that sweeps across the changes.
fn bench_per_core_steady_state(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();
	let mut group = c.benchmark_group("per_core_steady_state");
	group.measurement_time(Duration::from_secs(10));

	const TASKS: usize = 20_000;
	group.throughput(Throughput::Elements(TASKS as u64));
	group.bench_function("per_core", |b| {
		b.iter_custom(|iters| {
			rt.block_on(async move {
				let pool = Builder::new().thread_per_core(true).build();
				let _ = pool.spawn(|| 0u32).await;
				let mut total = Duration::ZERO;
				let counter = Arc::new(AtomicUsize::new(0));
				for _ in 0..iters {
					let start = Instant::now();
					let mut handles = Vec::with_capacity(TASKS);
					for i in 0..TASKS {
						let counter = counter.clone();
						handles.push(pool.spawn(move || {
							let mut s = i;
							for _ in 0..8 {
								s = s.wrapping_mul(17).wrapping_add(1);
							}
							counter.fetch_add(s, Ordering::Relaxed);
							s
						}));
					}
					for h in handles {
						black_box(h.await);
					}
					total += start.elapsed();
				}
				black_box(counter.load(Ordering::Relaxed));
				total
			})
		});
	});
	group.finish();
}

criterion_group!(
	microbench,
	bench_spawn_overhead,
	bench_steady_state_busy,
	bench_park_unpark_handshake,
	bench_multi_producer_contention,
	bench_spawn_local_overhead,
	bench_steal_imbalance,
	bench_per_core_steady_state,
);
criterion_main!(microbench);
