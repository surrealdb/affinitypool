#!/bin/bash

# Run affinitypool benchmarks
# Usage: ./run_benchmarks.sh [benchmark_name]

set -e

echo "Running affinitypool benchmarks..."
echo "System information:"
echo "- CPU cores: $(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 'unknown')"
echo "- Rust version: $(rustc --version)"

if [ $# -eq 0 ]; then
    echo ""
    echo "Running all benchmarks..."
    cargo bench --bench threadpool
else
    echo ""
    echo "Running benchmark: $1"
    cargo bench --bench threadpool -- "$1"
fi

echo ""
echo "Benchmark results are saved to target/criterion/"
echo "Open target/criterion/report/index.html in your browser to view detailed results"
