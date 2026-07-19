#!/usr/bin/env bash
# interop-test: Rust writes an index -> Java CheckIndex + query verification.
# Usage: interop/verify-index.sh [numDocs] [docBytes] [seed]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
NUM_DOCS="${1:-2000}"
DOC_BYTES="${2:-200}"
SEED="${3:-42}"
INDEX_DIR=/tmp/rustlucene-interop
GOLDEN=/tmp/rustlucene-interop-golden.txt
CP="$ROOT/interop/java/classes:$ROOT/interop/java/lib/lucene-core-9.12.3.jar:$ROOT/interop/java/lib/lucene-analysis-common-9.12.3.jar"

rm -rf "$INDEX_DIR" "$GOLDEN"
mkdir -p "$INDEX_DIR"

echo "== Rust: write index ($NUM_DOCS docs x $DOC_BYTES bytes, seed $SEED)"
cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
  write "$INDEX_DIR" "$NUM_DOCS" "$DOC_BYTES" "$SEED" "$GOLDEN"

echo "== No .pos/.pay files must exist"
if ls "$INDEX_DIR" | grep -qE '\.(pos|pay)$'; then
  echo "FAIL: .pos/.pay file found"; exit 1
fi

echo "== Java: CheckIndex"
java -cp "$CP" org.apache.lucene.index.CheckIndex "$INDEX_DIR" 2>&1 | grep -E "No problems|FAILED" || true
java -cp "$CP" org.apache.lucene.index.CheckIndex "$INDEX_DIR" > /dev/null 2>&1

echo "== Java: VerifyIndex (stored fields + ConstantScoreQuery term postings)"
java -cp "$CP" VerifyIndex "$INDEX_DIR" "$GOLDEN"

echo "INTEROP_OK"
