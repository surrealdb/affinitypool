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
