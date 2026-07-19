#!/usr/bin/env bash
# Rust vs Java write-throughput comparison, same corpus + same config.
# Usage: bench/run-bench.sh [numDocs] [docBytes] [threads] [rounds] [seed]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DOCS="${1:-1000000}"
BYTES="${2:-200}"
THREADS="${3:-1}"
ROUNDS="${4:-3}"
SEED="${5:-42}"
CP="$ROOT/interop/java/classes:$ROOT/interop/java/lib/lucene-core-9.12.3.jar:$ROOT/interop/java/lib/lucene-analysis-common-9.12.3.jar"
CLI="$ROOT/target/release/rustlucene-cli"

cd "$ROOT"
cargo build -q --release -p rustlucene-core

run_rust() {
  rm -rf /tmp/bench-rust; mkdir -p /tmp/bench-rust
  "$CLI" bench /tmp/bench-rust "$DOCS" "$BYTES" "$SEED" "$THREADS"
}
run_java() {
  rm -rf /tmp/bench-java; mkdir -p /tmp/bench-java
  java -Xmx4g -cp "$CP" JavaLuceneBench /tmp/bench-java "$DOCS" "$BYTES" "$THREADS" "$SEED" 2>/dev/null | grep '^BENCH'
}

best() { # pick max docs_per_sec among rounds
  sort -t= -k3 -rn | head -1
}

echo "docs=$DOCS doc_bytes=$BYTES threads=$THREADS rounds=$ROUNDS seed=$SEED"
for i in $(seq 1 "$ROUNDS"); do run_rust; done | tee /tmp/bench-rust-rounds.txt | best > /tmp/bench-rust-best.txt
for i in $(seq 1 "$ROUNDS"); do run_java; done | tee /tmp/bench-java-rounds.txt | best > /tmp/bench-java-best.txt

R=$(cat /tmp/bench-rust-best.txt); J=$(cat /tmp/bench-java-best.txt)
echo "RUST_BEST $R"
echo "JAVA_BEST $J"
awk -v r="$R" -v j="$J" 'BEGIN{
  match(r,/docs_per_sec=([0-9.]+)/,rd); match(j,/docs_per_sec=([0-9.]+)/,jd);
  match(r,/mb_per_sec=([0-9.]+)/,rm); match(j,/mb_per_sec=([0-9.]+)/,jm);
  printf "SPEEDUP docs/s: %.2fx   MB/s: %.2fx\n", rd[1]/jd[1], rm[1]/jm[1];
}'