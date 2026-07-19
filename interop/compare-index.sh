#!/usr/bin/env bash
# compare-index: build the SAME corpus into two indexes — one with the Rust
# writer (rustlucene-cli index), one with stock Java Lucene (JavaIndex) —
# then compare build time, CPU/memory usage, on-disk size, and term-level
# equality (a random SAMPLE_PCT% of the term dictionary, full doc lists).
#
# Usage: interop/compare-index.sh <inputFileOrDir> [numDocs] [--positions]
# Env:   SAMPLE_PCT (default 10), RUST_DIR, JAVA_DIR
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
INPUT="${1:?usage: compare-index.sh <inputFileOrDir> [numDocs] [--positions]}"
DOCS="${2:-}"
POSITIONS="${3:-}"
RUST_DIR="${RUST_DIR:-/tmp/compare-rust}"
JAVA_DIR="${JAVA_DIR:-/tmp/compare-java}"
SAMPLE_PCT="${SAMPLE_PCT:-10}"
CP="$ROOT/interop/java/classes:$ROOT/interop/java/lib/lucene-core-9.12.3.jar:$ROOT/interop/java/lib/lucene-analysis-common-9.12.3.jar"
TIME_FMT='%e %U %S %M %P'   # wall_s user_s sys_s maxrss_kb cpu%

FLAGS=()
[[ -n "$DOCS" ]] && FLAGS+=(--docs "$DOCS")
[[ "$POSITIONS" == "--positions" ]] && FLAGS+=(--positions)

cd "$ROOT"

echo "== build"
cargo build -q --release -p rustlucene-core
mkdir -p interop/java/classes
javac -cp "$CP" -d interop/java/classes interop/java/*.java

rm -rf "$RUST_DIR" "$JAVA_DIR"
mkdir -p "$RUST_DIR" "$JAVA_DIR"

get() { grep -oE "$2=[0-9]+" <<<"$1" | head -1 | cut -d= -f2; }

echo "== Rust write ($INPUT ${FLAGS[*]:-once})"
R_OUT=$(/usr/bin/time -f "$TIME_FMT" -o /tmp/compare-time-rust.txt \
  target/release/rustlucene-cli index "$INPUT" "$RUST_DIR" "${FLAGS[@]}")
echo "$R_OUT"

echo "== Java write ($INPUT ${FLAGS[*]:-once})"
J_OUT=$(/usr/bin/time -f "$TIME_FMT" -o /tmp/compare-time-java.txt \
  java -cp "$CP" JavaIndex "$INPUT" "$JAVA_DIR" "${FLAGS[@]}")
echo "$J_OUT"

read -r R_WALL R_USER R_SYS R_RSS R_CPUPCT < /tmp/compare-time-rust.txt
read -r J_WALL J_USER J_SYS J_RSS J_CPUPCT < /tmp/compare-time-java.txt
R_MS=$(get "$R_OUT" elapsed_ms); R_DPS=$(get "$R_OUT" docs_per_sec); R_DOCS=$(get "$R_OUT" docs)
J_MS=$(get "$J_OUT" elapsed_ms); J_DPS=$(get "$J_OUT" docs_per_sec); J_DOCS=$(get "$J_OUT" docs)
R_BYTES=$(du -sb "$RUST_DIR" | cut -f1)
J_BYTES=$(du -sb "$JAVA_DIR" | cut -f1)

echo "== CheckIndex (rust)"
java -cp "$CP" org.apache.lucene.index.CheckIndex "$RUST_DIR" 2>&1 | grep -E "No problems|FAILED" || true
java -cp "$CP" org.apache.lucene.index.CheckIndex "$RUST_DIR" > /dev/null 2>&1

echo "== CheckIndex (java)"
java -cp "$CP" org.apache.lucene.index.CheckIndex "$JAVA_DIR" 2>&1 | grep -E "No problems|FAILED" || true
java -cp "$CP" org.apache.lucene.index.CheckIndex "$JAVA_DIR" > /dev/null 2>&1

echo "== SUMMARY"
awk -v rd="$R_DOCS"  -v rm="$R_MS"  -v rw="$R_WALL" -v ru="$R_USER" -v rs="$R_SYS" -v rr="$R_RSS" -v rp="$R_CPUPCT" -v rdps="$R_DPS" -v rb="$R_BYTES" \
    -v jd="$J_DOCS"  -v jm="$J_MS"  -v jw="$J_WALL" -v ju="$J_USER" -v js="$J_SYS" -v jr="$J_RSS" -v jp="$J_CPUPCT" -v jdps="$J_DPS" -v jb="$J_BYTES" '
BEGIN {
  printf "%-6s %8s %10s %8s %9s %9s %6s %10s %12s %10s\n", \
    "writer", "docs", "elapsed_ms", "wall_s", "cpu_user", "cpu_sys", "cpu%", "maxrss_mb", "docs_per_sec", "dir_bytes"
  printf "%-6s %8d %10d %8.2f %9.2f %9.2f %6s %10.1f %12d %10d\n", "rust", rd, rm, rw, ru, rs, rp, rr/1024, rdps, rb
  printf "%-6s %8d %10d %8.2f %9.2f %9.2f %6s %10.1f %12d %10d\n", "java", jd, jm, jw, ju, js, jp, jr/1024, jdps, jb
  printf "speedup: throughput %.2fx | wall %.2fx | cpu_total %.2fx | rss java/rust %.2fx | dir_size java/rust %.2fx\n", \
    rdps/jdps, jw/rw, (ju+js)/(ru+rs), jr/rr, jb/rb
}'

echo "== Compare term postings (random ${SAMPLE_PCT}% of dictionary)"
java -cp "$CP" CompareIndexes "$RUST_DIR" "$JAVA_DIR" message "$SAMPLE_PCT" 42

echo "COMPARE_INDEXES_OK"
