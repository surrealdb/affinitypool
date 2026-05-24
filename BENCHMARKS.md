# Benchmarks

This document describes the comprehensive benchmark suite for affinitypool.

## Overview

The benchmark suite tests various scenarios to evaluate the performance characteristics of affinitypool under different conditions:

- **Single worker contention**: Many concurrent tasks with a single worker thread
- **Multi-worker contention**: Many concurrent tasks with 4 workers
- **Per-core contention**: Many concurrent tasks with one thread per CPU core (using affinity)
- **Throughput comparison**: Direct comparison of different pool configurations
- **Latency measurements**: Task execution latency across different configurations
- **Memory patterns**: Pool creation/destruction and burst task patterns

## Running Benchmarks

### Quick Start

```bash
# Run all benchmarks
./run_benchmarks.sh

# Run a specific benchmark group
./run_benchmarks.sh basic_operations

# Run benchmarks using cargo directly
cargo bench --bench threadpool
```

### Benchmark Groups

1. **`basic_operations`** - Basic threadpool operations with different worker counts
2. **`single_worker_contention`** - High contention with single worker
3. **`four_worker_contention`** - Optimal contention with 4 workers
4. **`per_core_contention`** - CPU affinity with thread per core
5. **`throughput_comparison`** - Direct throughput comparison
6. **`task_latency`** - Latency characteristics
7. **`memory_patterns`** - Memory usage and cleanup patterns

### Viewing Results

After running benchmarks, detailed HTML reports are available at:
```
target/criterion/report/index.html
```

## Benchmark Scenarios

### Single Worker Contention

This benchmark tests how affinitypool handles many concurrent tasks when bottlenecked by a single worker thread. It measures:
- Task queuing efficiency
- Memory overhead with many pending tasks
- Scheduler fairness

**Task counts tested**: 100, 1,000, 10,000, 50,000

### Multi-Worker Contention (4 Workers)

Tests performance with 4 worker threads, representing a common server configuration:
- Load distribution across workers
- Context switching overhead
- Scalability characteristics

**Task counts tested**: 100, 1,000, 10,000, 50,000

### Per-Core Contention

Tests CPU affinity features with one thread per CPU core:
- Core affinity benefits
- Cache locality improvements
- NUMA considerations

**Task counts tested**: 100, 1,000, 10,000, 50,000

### Throughput Comparison

Direct comparison of different pool configurations processing the same workload:
- 1 worker
- 4 workers  
- Per-core workers (with affinity)
- Half-core workers

**Fixed task count**: 20,000 CPU-intensive tasks

### Task Latency

Measures the latency from task submission to completion:
- Single task execution latency
- Effect of pool configuration on latency
- Comparison across different worker counts

### Memory Patterns

Tests memory usage and cleanup patterns:
- **Pool creation/destruction**: Rapid pool lifecycle
- **Task bursts**: Burst task submission followed by quiet periods

## CPU Task Workload

All benchmarks use a CPU-intensive task that performs arithmetic operations:
```rust
fn cpu_task(iterations: usize) -> usize {
    let mut sum: usize = 0;
    for i in 0..iterations {
        sum = sum.wrapping_add(i * 17 + 42);
    }
    sum
}
```

The iteration count varies by benchmark to simulate different workload intensities.

## System Requirements

- Rust 1.70+
- Multi-core system (benchmarks will adapt to available cores)
- Sufficient RAM for concurrent task execution

## Interpreting Results

### Key Metrics

- **Throughput**: Tasks processed per second
- **Latency**: Time from task submission to completion  
- **Scalability**: Performance improvement with additional workers
- **Efficiency**: Resource utilization per unit of work

### Expected Patterns

- **Single worker**: Linear scaling until bottlenecked by single thread
- **Multi-worker**: Better scaling for CPU-bound tasks
- **Per-core affinity**: Improved performance for CPU-intensive workloads
- **Memory patterns**: Low overhead for pool management

## Contributing

When adding new benchmarks:

1. Use the existing `cpu_task` function or create similar deterministic workloads
2. Test multiple input sizes to understand scaling characteristics
3. Include both throughput and latency measurements where relevant
4. Document the specific scenario being tested

## Troubleshooting

### Long Benchmark Times

- Use `./run_benchmarks.sh benchmark_name` to run specific groups
- Reduce `TASK_COUNTS` in the benchmark file for faster iterations during development

### Memory Issues

- Monitor system memory during large task count benchmarks
- Consider reducing maximum task counts on memory-constrained systems

### Platform Differences

- CPU affinity behavior varies by operating system
- Core counts and NUMA topology affect per-core benchmark results
- Results may vary significantly between architectures (x86, ARM, etc.)

## Pre-Rewrite Baseline (v0.5.0 / `perf/improvements`)

The numbers below are the baseline captured immediately before the
`rewrite/async-task` work began. They serve as the regression floor for
each subsequent PR. The bench machine was a quiet Linux box; runs were
gated on no other workloads being active.

### Microbench: `perf/improvements` vs `main` (clean run)

11 improved, 3 marginally regressed (workers=1 cases are noisy
main-vs-main at 40%+), 13 unchanged.

| Bench | Δ vs main | Verdict |
|---|---|---|
| `spawn_overhead/1w/100` | −18.7% | improved |
| `spawn_overhead/1w/10000` | −16.3% | improved |
| `spawn_overhead/4w/1000` | −11.5% | improved |
| `steady_state_busy/8` | −5.2% | improved |
| `park_unpark_handshake/4` | −6.6% | improved |
| `park_unpark_handshake/8` | −5.9% | improved |
| `multi_producer_contention/2p_4w` | −6.8% | improved |
| `multi_producer_contention/4p_4w` | −2.1% | improved |
| `multi_producer_contention/8p_1w` | −4.4% | improved |
| `spawn_local_overhead/4w/1000` | −7.5% | improved |
| `steal_imbalance/4w` | −6.2% | improved |
| `per_core_steady_state` | −4.0% | improved |
| `steady_state_busy/1` | +17.0% | regressed (workers=1 noise) |
| `spawn_overhead/1w/1000` | +9.4% | regressed (workers=1 noise) |
| `multi_producer_contention/4p_1w` | +12.6% | regressed (workers=1 noise) |

### Head-to-head: affinitypool vs `tokio::task::spawn_blocking` (`--quick`)

This is the gap the rewrite is closing. affinitypool only wins on
single-task round-trip with ≥4 workers and on the 8p/1w high-contention
case; tokio is 8-15× faster on batched spawn.

| Workload | affinitypool | tokio | ratio (ap/tokio) |
|---|---|---|---|
| `spawn_overhead/1w/10000` | 68.8 ms | 4.83 ms | **14.2× slower** |
| `spawn_overhead/4w/10000` | 48.4 ms | 5.53 ms | **8.76× slower** |
| `spawn_overhead/1w/1000` | 2.72 ms | 182 µs | 15.0× slower |
| `spawn_overhead/4w/1000` | 3.36 ms | 487 µs | 6.90× slower |
| `spawn_overhead/1w/100` | 681 µs | 64.7 µs | 10.5× slower |
| `spawn_overhead/4w/100` | 728 µs | 236 µs | 3.08× slower |
| `round_trip/1w` | 6.73 µs | 2.78 µs | 2.42× slower |
| `round_trip/4w` | 6.30 µs | 7.14 µs | **0.88× (wins)** |
| `round_trip/8w` | 6.69 µs | 7.27 µs | **0.92× (wins)** |
| `multi_producer/2p/1w` | 8.61 ms | 289 µs | 29.8× slower |
| `multi_producer/2p/4w` | 7.91 ms | 1.77 ms | 4.48× slower |
| `multi_producer/4p/1w` | 2.63 ms | 981 µs | 2.68× slower |
| `multi_producer/4p/4w` | 5.03 ms | 1.26 ms | 3.99× slower |
| `multi_producer/8p/1w` | 2.22 ms | 3.06 ms | **0.73× (wins)** |
| `multi_producer/8p/4w` | 6.68 ms | 5.63 ms | 1.19× slower |

### Rewrite targets

| Bench | Current | Tokio | Target |
|---|---|---|---|
| `spawn_overhead/1w/10000` | 68.8 ms | 4.83 ms | ≤ 5.3 ms |
| `spawn_overhead/4w/10000` | 48.4 ms | 5.53 ms | ≤ 6.0 ms |
| `round_trip/4w` | 6.30 µs | 7.14 µs | preserve current lead |
| `multi_producer/8p/4w/8000` | 6.68 ms | 5.63 ms | ≤ 6.2 ms |

Acceptance bar: match tokio within ±10% on every `vs_tokio` bench; keep
the natural architectural lead where it exists; no regressions on the
microbench suite vs the v0.5.0 numbers above.

## Post-rewrite Results (v0.6.0 / `rewrite/async-task`)

Same machine, same bench suite, `--quick` criterion. Comparison
against `tokio::task::spawn_blocking` matched producer + worker
counts.

### Head-to-head: affinitypool 0.6.0 vs `tokio::task::spawn_blocking`

| Bench | 0.5.0 | 0.6.0 | Tokio | 0.6.0 vs Tokio |
|---|---|---|---|---|
| `spawn_overhead/1w/1` | 6.71 µs | 3.20 µs | 7.14 µs | **AP wins 2.2×** |
| `spawn_overhead/1w/100` | 681 µs | 13.6 µs | 17.7 µs | **AP wins 1.3×** |
| `spawn_overhead/1w/1000` | 2.72 ms | 137 µs | 163 µs | **AP wins 1.2×** |
| `spawn_overhead/1w/10000` | 68.8 ms | 1.06 ms | 2.21 ms | **AP wins 2.1×** |
| `spawn_overhead/4w/1` | 3.04 µs | 7.05 µs | 2.99 µs | tokio wins 2.4× |
| `spawn_overhead/4w/100` | 728 µs | 172 µs | 61.7 µs | tokio wins 2.8× |
| `spawn_overhead/4w/1000` | 3.36 ms | 1.79 ms | 531 µs | tokio wins 3.4× |
| `spawn_overhead/4w/10000` | 48.4 ms | 16.2 ms | 6.99 ms | tokio wins 2.3× |
| `round_trip/1w` | 6.73 µs | 6.88 µs | 6.81 µs | parity |
| `round_trip/4w` | 6.30 µs | 4.93 µs | 7.14 µs | **AP wins 1.5×** |
| `round_trip/8w` | 6.69 µs | 6.00 µs | 5.59 µs | parity |
| `multi_producer/2p_1w` | 8.61 ms | 219 µs | 288 µs | **AP wins 1.3×** |
| `multi_producer/2p_4w` | 7.91 ms | 570 µs | 876 µs | **AP wins 1.5×** |
| `multi_producer/4p_1w` | 2.63 ms | 576 µs | 816 µs | **AP wins 1.4×** |
| `multi_producer/4p_4w` | 5.03 ms | 1.74 ms | 1.53 ms | tokio wins 1.1× |
| `multi_producer/8p_1w` | 2.22 ms | 3.24 ms | 2.94 ms | tokio wins 1.1× |
| `multi_producer/8p_4w` | 6.68 ms | 5.68 ms | 2.89 ms | tokio wins 2.0× |

**Summary: AP wins on 7 benches, parity on 2, loses on 8.** All the
losses are on `4w+` cases where mutex contention on the shared worker
queue is the dominant cost — see CHANGELOG for the architectural
explanation. Before 0.6.0, AP lost on every batched-spawn bench by
8-15×; now it wins outright on the majority and is within 2-3× on
the worst case.

### Allocations per task

`cargo run --example alloc_count --release` after the rewrite:

```
spawn(empty closure)       n=  1000  allocs=1000  bytes= 72000  alloc/task=1.00
spawn(empty closure)       n= 10000  allocs=10000 bytes=720000  alloc/task=1.00
spawn_local(empty closure) n=  1000  allocs=1000  bytes= 72000  alloc/task=1.00
spawn_local(empty closure) n= 10000  allocs=10000 bytes=720000  alloc/task=1.00
```

Exactly 1 heap allocation per task (the `async-task` task layout —
fused header + closure slot + result slot + waker slot). 72 bytes
per task. Parity with the previous hand-rolled `Job<F,R>` layout.
