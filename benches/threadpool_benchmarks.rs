use affinitypool::{Builder, Threadpool};
use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;

const TASK_COUNTS: &[usize] = &[100, 1000, 10000, 50000];
const WORKER_COUNTS: &[usize] = &[1, 2, 4, 8];

/// A simple CPU-intensive task for benchmarking
fn cpu_task(iterations: usize) -> usize {
	let mut sum: usize = 0;
	for i in 0..iterations {
		sum = sum.wrapping_add(i * 17 + 42);
	}
	sum
}

/// Benchmark basic threadpool operations with different worker counts
fn bench_basic_operations(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	let mut group = c.benchmark_group("basic_operations");

	for &workers in WORKER_COUNTS {
		for &task_count in &[100, 1000, 10000] {
			group.throughput(Throughput::Elements(task_count as u64));

			group.bench_with_input(
				BenchmarkId::new(format!("{}_workers", workers), task_count),
				&(workers, task_count),
				|b, &(workers, task_count)| {
					b.iter_custom(|iters| {
						rt.block_on(async move {
							let mut total_duration = Duration::from_nanos(0);

							for _iter in 0..iters {
								let pool = Threadpool::new(workers);
								let start = Instant::now();

								let mut handles = Vec::with_capacity(task_count);
								for _ in 0..task_count {
									handles.push(pool.spawn(|| black_box(cpu_task(100))));
								}

								for handle in handles {
									black_box(handle.await);
								}

								total_duration += start.elapsed();
							}

							total_duration
						})
					});
				},
			);
		}
	}
	group.finish();
}

/// Benchmark high contention scenario - many concurrent tasks with single worker
fn bench_single_worker_contention(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	let mut group = c.benchmark_group("single_worker_contention");
	group.measurement_time(Duration::from_secs(10));

	for &task_count in TASK_COUNTS {
		group.throughput(Throughput::Elements(task_count as u64));

		group.bench_with_input(
			BenchmarkId::new("single_worker", task_count),
			&task_count,
			|b, &task_count| {
				b.iter_custom(|iters| {
					rt.block_on(async move {
						let mut total_duration = Duration::from_nanos(0);

						for _iter in 0..iters {
							let pool = Threadpool::new(1);
							let counter = Arc::new(AtomicUsize::new(0));
							let start = Instant::now();

							let mut handles = Vec::with_capacity(task_count);
							for i in 0..task_count {
								let counter_clone = counter.clone();
								handles.push(pool.spawn(move || {
									// Mix of CPU work and atomic operations to simulate real work
									let result = cpu_task(50 + (i % 100));
									counter_clone.fetch_add(result, Ordering::Relaxed);
									result
								}));
							}

							for handle in handles {
								black_box(handle.await);
							}

							black_box(counter.load(Ordering::Relaxed));
							total_duration += start.elapsed();
						}

						total_duration
					})
				});
			},
		);
	}
	group.finish();
}

/// Benchmark optimal contention scenario - many concurrent tasks with 4 workers
fn bench_multi_worker_contention(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	let mut group = c.benchmark_group("four_worker_contention");
	group.measurement_time(Duration::from_secs(10));

	for &task_count in TASK_COUNTS {
		group.throughput(Throughput::Elements(task_count as u64));

		group.bench_with_input(
			BenchmarkId::new("four_workers", task_count),
			&task_count,
			|b, &task_count| {
				b.iter_custom(|iters| {
					rt.block_on(async move {
						let mut total_duration = Duration::from_nanos(0);

						for _iter in 0..iters {
							let pool = Threadpool::new(4);
							let counter = Arc::new(AtomicUsize::new(0));
							let start = Instant::now();

							let mut handles = Vec::with_capacity(task_count);
							for i in 0..task_count {
								let counter_clone = counter.clone();
								handles.push(pool.spawn(move || {
									// Mix of CPU work and atomic operations
									let result = cpu_task(50 + (i % 100));
									counter_clone.fetch_add(result, Ordering::Relaxed);
									result
								}));
							}

							for handle in handles {
								black_box(handle.await);
							}

							black_box(counter.load(Ordering::Relaxed));
							total_duration += start.elapsed();
						}

						total_duration
					})
				});
			},
		);
	}
	group.finish();
}

/// Benchmark per-core affinity scenario - many concurrent tasks with thread per core
fn bench_per_core_contention(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();
	let num_cores = num_cpus::get();

	let mut group = c.benchmark_group("per_core_contention");
	group.measurement_time(Duration::from_secs(10));

	for &task_count in TASK_COUNTS {
		group.throughput(Throughput::Elements(task_count as u64));

		group.bench_with_input(
			BenchmarkId::new(format!("{}_cores", num_cores), task_count),
			&task_count,
			|b, &task_count| {
				b.iter_custom(|iters| {
					rt.block_on(async move {
						let mut total_duration = Duration::from_nanos(0);

						for _iter in 0..iters {
							let pool = Builder::new().thread_per_core(true).build();
							let counter = Arc::new(AtomicUsize::new(0));
							let start = Instant::now();

							let mut handles = Vec::with_capacity(task_count);
							for i in 0..task_count {
								let counter_clone = counter.clone();
								handles.push(pool.spawn(move || {
									// CPU-intensive work that benefits from core affinity
									let result = cpu_task(75 + (i % 150));
									counter_clone.fetch_add(result, Ordering::Relaxed);
									result
								}));
							}

							for handle in handles {
								black_box(handle.await);
							}

							black_box(counter.load(Ordering::Relaxed));
							total_duration += start.elapsed();
						}

						total_duration
					})
				});
			},
		);
	}
	group.finish();
}

/// Benchmark throughput comparison across different pool configurations
fn bench_throughput_comparison(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();
	let num_cores = num_cpus::get();

	let mut group = c.benchmark_group("throughput_comparison");
	group.measurement_time(Duration::from_secs(15));

	const THROUGHPUT_TASKS: usize = 20000;
	group.throughput(Throughput::Elements(THROUGHPUT_TASKS as u64));

	// Single worker
	group.bench_function("1_worker", |b| {
		b.iter_custom(|iters| {
			rt.block_on(async move {
				let mut total_duration = Duration::from_nanos(0);

				for _iter in 0..iters {
					let pool = Threadpool::new(1);
					let start = Instant::now();

					let mut handles = Vec::with_capacity(THROUGHPUT_TASKS);
					for i in 0..THROUGHPUT_TASKS {
						handles.push(pool.spawn(move || cpu_task(25 + (i % 50))));
					}

					for handle in handles {
						black_box(handle.await);
					}

					total_duration += start.elapsed();
				}

				total_duration
			})
		});
	});

	// Four workers
	group.bench_function("4_workers", |b| {
		b.iter_custom(|iters| {
			rt.block_on(async move {
				let mut total_duration = Duration::from_nanos(0);

				for _iter in 0..iters {
					let pool = Threadpool::new(4);
					let start = Instant::now();

					let mut handles = Vec::with_capacity(THROUGHPUT_TASKS);
					for i in 0..THROUGHPUT_TASKS {
						handles.push(pool.spawn(move || cpu_task(25 + (i % 50))));
					}

					for handle in handles {
						black_box(handle.await);
					}

					total_duration += start.elapsed();
				}

				total_duration
			})
		});
	});

	// Per-core workers
	group.bench_function(format!("{}_cores_affinity", num_cores).as_str(), |b| {
		b.iter_custom(|iters| {
			rt.block_on(async move {
				let mut total_duration = Duration::from_nanos(0);

				for _iter in 0..iters {
					let pool = Builder::new().thread_per_core(true).build();
					let start = Instant::now();

					let mut handles = Vec::with_capacity(THROUGHPUT_TASKS);
					for i in 0..THROUGHPUT_TASKS {
						handles.push(pool.spawn(move || cpu_task(25 + (i % 50))));
					}

					for handle in handles {
						black_box(handle.await);
					}

					total_duration += start.elapsed();
				}

				total_duration
			})
		});
	});

	// Half cores (for comparison)
	let half_cores = (num_cores / 2).max(1);
	group.bench_function(format!("{}_workers", half_cores).as_str(), |b| {
		b.iter_custom(|iters| {
			rt.block_on(async move {
				let mut total_duration = Duration::from_nanos(0);

				for _iter in 0..iters {
					let pool = Threadpool::new(half_cores);
					let start = Instant::now();

					let mut handles = Vec::with_capacity(THROUGHPUT_TASKS);
					for i in 0..THROUGHPUT_TASKS {
						handles.push(pool.spawn(move || cpu_task(25 + (i % 50))));
					}

					for handle in handles {
						black_box(handle.await);
					}

					total_duration += start.elapsed();
				}

				total_duration
			})
		});
	});

	group.finish();
}

/// Benchmark latency characteristics of different pool configurations
fn bench_task_latency(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	let mut group = c.benchmark_group("task_latency");

	// Test latency with different pool configurations
	let configs = vec![
		("single_worker", 1, false),
		("four_workers", 4, false),
		("per_core_affinity", 0, true), // 0 will be ignored when thread_per_core is true
	];

	for (name, workers, per_core) in configs {
		group.bench_function(name, |b| {
			b.iter_custom(|iters| {
				rt.block_on(async move {
					let mut total_duration = Duration::from_nanos(0);

					for _iter in 0..iters {
						let pool = if per_core {
							Builder::new().thread_per_core(true).build()
						} else {
							Threadpool::new(workers)
						};

						// Measure latency of single task execution
						let start = Instant::now();
						let result = pool.spawn(|| cpu_task(100)).await;
						let latency = start.elapsed();
						total_duration += latency;

						black_box(result);
					}

					total_duration
				})
			});
		});
	}

	group.finish();
}

/// Benchmark memory usage patterns and cleanup
fn bench_memory_patterns(c: &mut Criterion) {
	let rt = Runtime::new().unwrap();

	let mut group = c.benchmark_group("memory_patterns");

	// Test rapid pool creation and destruction
	group.bench_function("pool_creation_destruction", |b| {
		b.iter_custom(|iters| {
			rt.block_on(async move {
				let mut total_duration = Duration::from_nanos(0);

				for _iter in 0..iters {
					let start = Instant::now();
					let pool = Builder::new().worker_threads(4).build();

					// Submit a few tasks to actually use the pool
					let mut handles = Vec::new();
					for _ in 0..10 {
						handles.push(pool.spawn(|| cpu_task(10)));
					}

					for handle in handles {
						black_box(handle.await);
					}

					// Pool drops here
					total_duration += start.elapsed();
				}

				total_duration
			})
		});
	});

	// Test task burst patterns
	group.bench_function("task_bursts", |b| {
		b.iter_custom(|iters| {
			rt.block_on(async move {
				let mut total_duration = Duration::from_nanos(0);

				for _iter in 0..iters {
					let pool = Threadpool::new(4);
					let start = Instant::now();

					// Simulate burst of tasks followed by quiet period
					for burst in 0..5 {
						let mut handles = Vec::new();
						for _ in 0..100 {
							handles.push(pool.spawn(move || cpu_task(20 + burst * 5)));
						}

						for handle in handles {
							black_box(handle.await);
						}

						// Small delay between bursts
						tokio::time::sleep(Duration::from_millis(1)).await;
					}

					total_duration += start.elapsed();
				}

				total_duration
			})
		});
	});

	group.finish();
}

criterion_group!(
	benches,
	bench_basic_operations,
	bench_single_worker_contention,
	bench_multi_worker_contention,
	bench_per_core_contention,
	bench_throughput_comparison,
	bench_task_latency,
	bench_memory_patterns
);

criterion_main!(benches);
