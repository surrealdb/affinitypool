#!/usr/bin/env bash
#
# Run the four head-to-head bench suites sequentially, waiting for the
# 1-minute Linux loadavg to drop below 1.0 between runs. Pre-compiles
# all four bench binaries up front so cargo's own work doesn't spike
# the load mid-suite.

set -uo pipefail

THRESHOLD=${LOAD_THRESHOLD:-1.0}
SLEEP=${LOAD_SLEEP:-30}
BENCHES=(vs_tokio vs_blocking vs_rayon vs_threadpool)

wait_for_low_load() {
	while :; do
		load=$(cut -d' ' -f1 /proc/loadavg)
		if awk -v l="$load" -v t="$THRESHOLD" 'BEGIN{exit !(l < t)}'; then
			return
		fi
		echo "load=$load >= $THRESHOLD, waiting ${SLEEP}s..."
		sleep "$SLEEP"
	done
}

bench_args=()
for b in "${BENCHES[@]}"; do
	bench_args+=(--bench "$b")
done

echo "=== compiling bench binaries ==="
cargo bench --no-run "${bench_args[@]}"

for b in "${BENCHES[@]}"; do
	wait_for_low_load
	echo "=== running $b (load=$load) ==="
	cargo bench --bench "$b"
done
