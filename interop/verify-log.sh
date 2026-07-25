#!/usr/bin/env bash
# M2/M6 interop: Rust writes a log-schema index -> Java CheckIndex + query dump;
# Java writes the same corpus with stock Lucene -> CheckIndex + query dump;
# the two dumps must be identical.
#
# Standard variants: [--positions|--sparse|--bigdict|--bitmap]
# Forcemerge variants (M6 Task C): [--forcemerge|--forcemerge-bitmap]
#   These write with --flush-every 10000 to create multiple segments, then
#   forceMerge(1) the Rust index in-process and forceMerge the Java baseline.
# Usage: interop/verify-log.sh [numDocs] [seed] [variant flags...]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
NUM_DOCS="${1:-200000}"
SEED="${2:-42}"
shift 2 || true

POSITIONS=""
BITMAP=""
SPARSE=""
BIGDICT=""
FORCEMERGE=false
FORCEMERGE_BITMAP=false

for f in "$@"; do
  case "$f" in
    --positions) POSITIONS="--positions" ;;
    --sparse)    SPARSE="--sparse" ;;
    --bigdict)   BIGDICT="--bigdict" ;;
    --bitmap)    BITMAP="--bitmap" ;;
    --forcemerge) FORCEMERGE=true ;;
    --forcemerge-bitmap)
      FORCEMERGE=true
      FORCEMERGE_BITMAP=true
      BITMAP="--bitmap"
      ;;
    *)
      echo "unknown variant flag: $f"
      echo "usage: $0 [numDocs] [seed] [--positions|--sparse|--bigdict|--bitmap|--forcemerge|--forcemerge-bitmap]"
      exit 2
      ;;
  esac
done

WRITE_FLAGS=()
[ -n "$POSITIONS" ] && WRITE_FLAGS+=("$POSITIONS")
[ -n "$SPARSE" ]    && WRITE_FLAGS+=("$SPARSE")
[ -n "$BIGDICT" ]   && WRITE_FLAGS+=("$BIGDICT")
[ -n "$BITMAP" ]    && WRITE_FLAGS+=("$BITMAP")

RUST_DIR=/tmp/rl-log-rust
JAVA_DIR=/tmp/rl-log-java
CP="$ROOT/interop/java/classes:$ROOT/interop/java/lib/lucene-core-9.12.3.jar:$ROOT/interop/java/lib/lucene-analysis-common-9.12.3.jar"

rm -rf "$RUST_DIR" "$JAVA_DIR"
mkdir -p "$RUST_DIR" "$JAVA_DIR"

check_index() {
  local dir="$1"
  echo "== CheckIndex $dir"
  java -cp "$CP" org.apache.lucene.index.CheckIndex "$dir" 2>&1 \
    | grep -E "No problems|FAILED|error" || true
  java -cp "$CP" org.apache.lucene.index.CheckIndex "$dir" > /dev/null 2>&1
}

run_standard_variant() {
  echo "== Rust: logwrite ($NUM_DOCS docs, seed $SEED ${WRITE_FLAGS[*]+${WRITE_FLAGS[*]}})"
  cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
    logwrite "$RUST_DIR" "$NUM_DOCS" "$SEED" "${WRITE_FLAGS[@]}"

  echo "== Java: JavaLogBench (same corpus)"
  java -cp "$CP" JavaLogBench "$JAVA_DIR" "$NUM_DOCS" 1 "$SEED" "${WRITE_FLAGS[@]}"

  check_index "$RUST_DIR"
  check_index "$JAVA_DIR"

  echo "== VerifyLogIndex: Rust vs Java dumps"
  local expect_positions=false
  [ "$POSITIONS" = "--positions" ] && expect_positions=true
  java -cp "$CP" VerifyLogIndex "$RUST_DIR" "$expect_positions" > /tmp/rl-log-rust.out
  java -cp "$CP" VerifyLogIndex "$JAVA_DIR" "$expect_positions" > /tmp/rl-log-java.out
  diff -u /tmp/rl-log-rust.out /tmp/rl-log-java.out
  cat /tmp/rl-log-rust.out

  echo "== Search diff: searchdump vs VerifySearchIndex"
  "$ROOT/interop/verify-search.sh" "$RUST_DIR" "$JAVA_DIR" "$NUM_DOCS" "$SEED" "$POSITIONS"

  if [ "$BITMAP" = "--bitmap" ]; then
    echo "== Bitmap A/B: Rust searchdump bitmap on vs off (RL_BITMAP=0)"
    env -u RL_BITMAP cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
      searchdump "$RUST_DIR" "$NUM_DOCS" "$SEED" > /tmp/rl-search-bitmap-on.out
    RL_BITMAP=0 cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
      searchdump "$RUST_DIR" "$NUM_DOCS" "$SEED" > /tmp/rl-search-bitmap-off.out
    diff -u /tmp/rl-search-bitmap-on.out /tmp/rl-search-bitmap-off.out

    echo "== Java forceMerge on the Rust --bitmap index"
    java -cp "$CP" ForceMergeIndex "$RUST_DIR"
    echo "== CheckIndex post-merge $RUST_DIR"
    check_index "$RUST_DIR"

    echo "== Post-merge search diff (merged index carries no bitmaps)"
    "$ROOT/interop/verify-search.sh" "$RUST_DIR" "$JAVA_DIR" "$NUM_DOCS" "$SEED" ""
  fi

  echo "LOG_INTEROP_OK"
}

run_forcemerge_variant() {
  local flush_every=10000
  local rust_fm_flags=()
  $FORCEMERGE_BITMAP && rust_fm_flags+=("--bitmap")

  echo "== Rust: logwrite + forcemerge ($NUM_DOCS docs, seed $SEED, flush_every=$flush_every ${WRITE_FLAGS[*]+${WRITE_FLAGS[*]}}, merge=${rust_fm_flags[*]+${rust_fm_flags[*]}})"
  cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
    logwrite "$RUST_DIR" "$NUM_DOCS" "$SEED" "${WRITE_FLAGS[@]}" --flush-every "$flush_every"
  cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
    forcemerge "$RUST_DIR" "${rust_fm_flags[@]}"

  echo "== Java: JavaLogBench + ForceMergeIndex (same corpus)"
  java -cp "$CP" JavaLogBench "$JAVA_DIR" "$NUM_DOCS" 1 "$SEED" "${WRITE_FLAGS[@]}"
  java -cp "$CP" ForceMergeIndex "$JAVA_DIR"

  check_index "$RUST_DIR"
  check_index "$JAVA_DIR"

  echo "== VerifyLogIndex: Rust vs Java dumps (post-forceMerge)"
  local expect_positions=false
  [ "$POSITIONS" = "--positions" ] && expect_positions=true
  java -cp "$CP" VerifyLogIndex "$RUST_DIR" "$expect_positions" > /tmp/rl-log-rust.out
  java -cp "$CP" VerifyLogIndex "$JAVA_DIR" "$expect_positions" > /tmp/rl-log-java.out
  diff -u /tmp/rl-log-rust.out /tmp/rl-log-java.out
  cat /tmp/rl-log-rust.out

  echo "== Search diff: searchdump vs VerifySearchIndex (post-forceMerge)"
  "$ROOT/interop/verify-search.sh" "$RUST_DIR" "$JAVA_DIR" "$NUM_DOCS" "$SEED" "$POSITIONS"

  if $FORCEMERGE_BITMAP; then
    echo "== Bitmap A/B: Rust forcemerged bitmap on vs off (RL_BITMAP=0)"
    env -u RL_BITMAP cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
      searchdump "$RUST_DIR" "$NUM_DOCS" "$SEED" > /tmp/rl-search-bitmap-on.out
    RL_BITMAP=0 cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
      searchdump "$RUST_DIR" "$NUM_DOCS" "$SEED" > /tmp/rl-search-bitmap-off.out
    diff -u /tmp/rl-search-bitmap-on.out /tmp/rl-search-bitmap-off.out
  fi

  echo "FORCEMERGE_INTEROP_OK"
}

if $FORCEMERGE; then
  run_forcemerge_variant
else
  run_standard_variant
fi
