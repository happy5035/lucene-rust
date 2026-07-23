#!/usr/bin/env bash
# M1 search diff: rustlucene-cli searchdump (Rust read path) vs
# VerifySearchIndex (Java Lucene 9.12.3) over the same two indexes.
# The battery (term/matchall + Boolean and/or items) is hardcoded in both
# dumpers; this script diffs their full output line by line.
# Called by interop/verify-log.sh after both indexes are built.
# Usage: interop/verify-search.sh <rustIndexDir> <javaIndexDir> <numDocs> <seed> [positionsFlag]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RUST_DIR="$1"
JAVA_DIR="$2"
NUM_DOCS="$3"
SEED="$4"
POSITIONS="${5:-}"
CP="$ROOT/interop/java/classes:$ROOT/interop/java/lib/lucene-core-9.12.3.jar:$ROOT/interop/java/lib/lucene-analysis-common-9.12.3.jar"

echo "== Rust: searchdump"
cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
  searchdump "$RUST_DIR" "$NUM_DOCS" "$SEED" $POSITIONS > /tmp/rl-search-rust.out

echo "== Java: VerifySearchIndex"
java -cp "$CP" VerifySearchIndex "$JAVA_DIR" $POSITIONS > /tmp/rl-search-java.out

diff -u /tmp/rl-search-rust.out /tmp/rl-search-java.out
cat /tmp/rl-search-rust.out

echo "SEARCH_INTEROP_OK"
