#!/usr/bin/env bash
# M2 interop: Rust writes a log-schema index -> Java CheckIndex + query dump;
# Java writes the same corpus with stock Lucene -> CheckIndex + query dump;
# the two dumps must be identical.
# Usage: interop/verify-log.sh [numDocs] [seed] [--positions|--sparse|--bigdict|--bitmap]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
NUM_DOCS="${1:-200000}"
SEED="${2:-42}"
POSITIONS="${3:-}"
RUST_DIR=/tmp/rl-log-rust
JAVA_DIR=/tmp/rl-log-java
CP="$ROOT/interop/java/classes:$ROOT/interop/java/lib/lucene-core-9.12.3.jar:$ROOT/interop/java/lib/lucene-analysis-common-9.12.3.jar"

rm -rf "$RUST_DIR" "$JAVA_DIR"
mkdir -p "$RUST_DIR" "$JAVA_DIR"

echo "== Rust: logwrite ($NUM_DOCS docs, seed $SEED $POSITIONS)"
cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
  logwrite "$RUST_DIR" "$NUM_DOCS" "$SEED" $POSITIONS

echo "== Java: JavaLogBench (same corpus)"
java -cp "$CP" JavaLogBench "$JAVA_DIR" "$NUM_DOCS" 1 "$SEED" $POSITIONS
for side in "$RUST_DIR" "$JAVA_DIR"; do
  echo "== CheckIndex $side"
  java -cp "$CP" org.apache.lucene.index.CheckIndex "$side" 2>&1 \
    | grep -E "No problems|FAILED|error" || true
  java -cp "$CP" org.apache.lucene.index.CheckIndex "$side" > /dev/null 2>&1
done

echo "== VerifyLogIndex: Rust vs Java dumps"
EXPECT_POSITIONS=false
[ "$POSITIONS" = "--positions" ] && EXPECT_POSITIONS=true
java -cp "$CP" VerifyLogIndex "$RUST_DIR" "$EXPECT_POSITIONS" > /tmp/rl-log-rust.out
java -cp "$CP" VerifyLogIndex "$JAVA_DIR" "$EXPECT_POSITIONS" > /tmp/rl-log-java.out
diff -u /tmp/rl-log-rust.out /tmp/rl-log-java.out
cat /tmp/rl-log-rust.out

echo "== Search diff: searchdump vs VerifySearchIndex"
"$ROOT/interop/verify-search.sh" "$RUST_DIR" "$JAVA_DIR" "$NUM_DOCS" "$SEED" "$POSITIONS"

if [ "$POSITIONS" = "--bitmap" ]; then
  echo "== Bitmap A/B: Rust searchdump bitmap on vs off (RL_BITMAP=0)"
  env -u RL_BITMAP cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
    searchdump "$RUST_DIR" "$NUM_DOCS" "$SEED" > /tmp/rl-search-bitmap-on.out
  RL_BITMAP=0 cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
    searchdump "$RUST_DIR" "$NUM_DOCS" "$SEED" > /tmp/rl-search-bitmap-off.out
  diff -u /tmp/rl-search-bitmap-on.out /tmp/rl-search-bitmap-off.out

  echo "== Java forceMerge on the Rust --bitmap index"
  java -cp "$CP" ForceMergeIndex "$RUST_DIR"
  echo "== CheckIndex post-merge $RUST_DIR"
  java -cp "$CP" org.apache.lucene.index.CheckIndex "$RUST_DIR" 2>&1 \
    | grep -E "No problems|FAILED|error" || true
  java -cp "$CP" org.apache.lucene.index.CheckIndex "$RUST_DIR" > /dev/null 2>&1

  echo "== Post-merge search diff (merged index carries no bitmaps)"
  "$ROOT/interop/verify-search.sh" "$RUST_DIR" "$JAVA_DIR" "$NUM_DOCS" "$SEED" ""
fi

echo "LOG_INTEROP_OK"
