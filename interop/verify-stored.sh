#!/usr/bin/env bash
# End-to-end check: Rust writes a stored-only segment, Java Lucene 9.12.3
# reads it back (DirectoryReader + CheckIndex). Repeatable: cleans old output.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT_DIR=/tmp/rustlucene-m1a
JAR="$ROOT/interop/java/lib/lucene-core-9.12.3.jar"
CLASSES="$ROOT/interop/java/classes"

export PATH="$HOME/.cargo/bin:$PATH"

echo "==> clean old output"
rm -rf "$OUT_DIR" "$CLASSES"

echo "==> cargo run --example write_stored_only"
(cd "$ROOT" && cargo run -q -p codec-lucene9 --example write_stored_only)

echo "==> javac VerifyStored.java"
mkdir -p "$CLASSES"
javac -cp "$JAR" -d "$CLASSES" "$ROOT/interop/java/VerifyStored.java"

echo "==> java VerifyStored (DirectoryReader + CheckIndex)"
java -cp "$JAR:$CLASSES" VerifyStored

echo "==> SUCCESS"
