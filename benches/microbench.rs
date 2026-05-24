//! Microbenchmarks for the per-spawn hot path.
//!
//! These complement `benches/threadpool.rs` (which mixes spawn cost with
//! per-task compute) by isolating the components that the performance work
//! actually targets:
//!
//! * **`spawn_overhead`** — empty closure, single worker. Dominated by
//!   `async-task` allocation and the spawn/poll state machine.
//! * **`steady_state_busy`** — keep all workers busy with many tasks queued,
//!   so the parking machinery is never exercised. Isolates the shard push
//!   cost and the `parked.load(Acquire)` short-circuit on the producer hot
//!   path.
//! * **`park_unpark_handshake`** — submit a single task, await, then go
//!   idle. Forces a park/unpark cycle on each iteration. Stresses the
//!   arm-then-rescan handshake that protects against lost wakeups (see
//!   `src/queue.rs` and `tests/loom_queue.rs`).
//! * **`multi_producer_contention`** — many concurrent async producers fed
//!   into one pool. Stresses cross-shard contention and the `parked` atomic
//!   gate from the producer side.
//! * **`spawn_local_overhead`** — empty closure via `spawn_local`. The
//!   `spawn_local` path schedules lazily on first poll and runs the cancel
//!   path on drop, so it has different completion costs than `spawn` and is
//!   not covered by the existing benches.
//! * **`steal_imbalance`** — push every task from a single producer onto
//!   one shard with N workers idle, so peer workers have to scan empty
//!   shards before finding the work. Stresses the worker's shard-scan path
//!   plus the wake-fanout that distributes the queued work across the
//!   parked workers. (Name preserved for criterion history continuity —
//!   the current implementation is shard-scanning, not work-stealing.)

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
/// producer hot path (`spawn` → shard push → `parked.load` short-circuit)
/// and the worker hot path (Phase-1 shard scan loop) are both isolated.
fn bench_steady_state_busy(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();
	let mut group = c.benchmark_group("steady_state_busy");
	group.measurement_time(Duration::from_secs(10));

	const TASK_PER_WORKER: usize = 4096;
	for &workers in &[1usize, 4, 8] {
		let total_tasks = workers * TASK_PER_WORKER;
		group.throughput(Throughput::Elements(total_tasks as u64));
		group.bench_with_input(BenchmarkId::new("workers", workers), &workers, |b, &workers| {
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
						// will always find something on their own shard
						// or a peer shard and never park.
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
		});
	}
	group.finish();
}

/// One task per iteration, awaited before the next is submitted. The pool
/// goes fully idle between tasks, so each iteration exercises a full
/// park/unpark cycle. This is the path the arm-then-rescan handshake in
/// `src/queue.rs` exists to protect (see also the loom model in
/// `tests/loom_queue.rs`).
fn bench_park_unpark_handshake(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();
	let mut group = c.benchmark_group("park_unpark_handshake");
	group.measurement_time(Duration::from_secs(10));

	for &workers in &[1usize, 4, 8] {
		group.bench_with_input(BenchmarkId::new("workers", workers), &workers, |b, &workers| {
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
		});
	}
	group.finish();
}

/// N tokio tasks each submit `tasks_per_producer` jobs into the same pool
/// concurrently. Stresses cross-shard contention (each producer routes to
/// the shard matching its CPU) and the `parked` atomic gate from the
/// producer side.
fn bench_multi_producer_contention(c: &mut Criterion) {
	let rt =
		tokio::runtime::Builder::new_multi_thread().worker_threads(8).enable_all().build().unwrap();
	let mut group = c.benchmark_group("multi_producer_contention");
	group.measurement_time(Duration::from_secs(10));

	const TASKS_PER_PRODUCER: usize = 1_000;
	for &producers in &[2usize, 4, 8] {
		for &workers in &[1usize, 4] {
			let total = producers * TASKS_PER_PRODUCER;
			group.throughput(Throughput::Elements(total as u64));
			group.bench_with_input(
				BenchmarkId::new(format!("{}_producers_{}_workers", producers, workers), total),
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

/// Empty closure via `spawn_local`. Isolates the `spawn_local` cost
/// (lazy-schedule on first poll, drop-blocks-on-cancel) from the eager
/// `spawn` path.
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
/// batch onto one shard (the producer's CPU-routed shard) and awaits
/// completion. Stresses the worker shard-scan path that lets peer workers
/// discover the imbalanced load, plus the wake-fanout from the producer's
/// `parked > 0` notify path.
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
