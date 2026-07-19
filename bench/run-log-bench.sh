#!/usr/bin/env bash
# M4 log-scenario benchmark: Rust logbench vs JavaLogBench on the same
# synthetic log corpus (timestamp/level/trace_id/message/3 numeric DVs).
# Usage: bench/run-log-bench.sh [numDocs] [threads] [rounds] [seed] [--positions]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DOCS="${1:-1000000}"
THREADS="${2:-1}"
ROUNDS="${3:-3}"
SEED="${4:-42}"
POSITIONS="${5:-}"
CP="$ROOT/interop/java/classes:$ROOT/interop/java/lib/lucene-core-9.12.3.jar:$ROOT/interop/java/lib/lucene-analysis-common-9.12.3.jar"
CLI="$ROOT/target/release/rustlucene-cli"

cd "$ROOT"
cargo build -q --release -p rustlucene-core

run_rust() {
  rm -rf /tmp/bench-log-rust; mkdir -p /tmp/bench-log-rust
  "$CLI" logbench /tmp/bench-log-rust "$DOCS" "$SEED" "$THREADS" $POSITIONS
}
run_java() {
  rm -rf /tmp/bench-log-java; mkdir -p /tmp/bench-log-java
  java -Xmx4g -cp "$CP" JavaLogBench /tmp/bench-log-java "$DOCS" "$THREADS" "$SEED" $POSITIONS 2>/dev/null | grep '^BENCH'
}

best() { sort -t= -k3 -rn | head -1; }

echo "log-bench docs=$DOCS threads=$THREADS rounds=$ROUNDS seed=$SEED $POSITIONS"
for i in $(seq 1 "$ROUNDS"); do run_rust; done | tee /tmp/bench-log-rust-rounds.txt | best > /tmp/bench-log-rust-best.txt
for i in $(seq 1 "$ROUNDS"); do run_java; done | tee /tmp/bench-log-java-rounds.txt | best > /tmp/bench-log-java-best.txt

R=$(cat /tmp/bench-log-rust-best.txt); J=$(cat /tmp/bench-log-java-best.txt)
echo "RUST_BEST $R"
echo "JAVA_BEST $J"
awk -v r="$R" -v j="$J" 'BEGIN{
  match(r,/docs_per_sec=([0-9.]+)/,rd); match(j,/docs_per_sec=([0-9.]+)/,jd);
  match(r,/mb_per_sec=([0-9.]+)/,rm); match(j,/mb_per_sec=([0-9.]+)/,jm);
  printf "SPEEDUP docs/s: %.2fx   MB/s: %.2fx\n", rd[1]/jd[1], rm[1]/jm[1];
}'
