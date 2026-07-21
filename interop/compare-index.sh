#!/usr/bin/env bash
# compare-index: build the SAME corpus into two indexes — one with the Rust
# writer (rustlucene-cli index), one with stock Java Lucene (JavaIndex) —
# then compare build time, CPU/memory usage, on-disk size, and term-level
# equality (a random SAMPLE_PCT% of the term dictionary, full doc lists).
#
# Usage: interop/compare-index.sh <inputFileOrDir> [numDocs] [--positions] [--skip-build]
#        interop/compare-index.sh <corpusJsonl> [numDocs] [--skip-build] --json
# Env:   SAMPLE_PCT (default 10), RUST_DIR, JAVA_DIR, JNI_DIR, BATCH_SIZE,
#        COMPARE_SKIP_BUILD=1, JAVA_HOME
#
# --json mode: generates the JSONL corpus with `rustlucene-cli jsongen`
# (numDocs, default 200000), then builds three indexes of the same corpus —
# rust jsonindex, the JNI batch writer (JsonJniBench) and stock Java Lucene
# (JavaJsonIndex) — and runs the same timing/CheckIndex/term-diff pipeline
# on all three.
#
# Package mode: when the script detects a bundled bin/rustlucene-cli and
# java/ directory next to itself, it uses those pre-built artifacts and
# skips compilation entirely.  Build the package once with:
#   make package-compare
# or:
#   interop/package-compare.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# --- resolve java ---------------------------------------------------------
if [[ -n "${JAVA_HOME:-}" ]]; then
	JAVA="$JAVA_HOME/bin/java"
	JAVAC="$JAVA_HOME/bin/javac"
else
	JAVA="java"
	JAVAC="javac"
fi
if ! command -v "$JAVA" &>/dev/null; then
	echo "ERROR: java not found (searched: $JAVA); set JAVA_HOME" >&2
	exit 1
fi

# --- detect package mode --------------------------------------------------
# If bin/rustlucene-cli and java/ exist next to this script, we're in a
# packaged deployment — use bundled artifacts, no compilation needed.
PACKAGED=false
if [[ -x "$SCRIPT_DIR/bin/rustlucene-cli" ]] && [[ -d "$SCRIPT_DIR/java" ]]; then
	PACKAGED=true
fi

# --- parse args -----------------------------------------------------------
POSITIONS=""
SKIP_BUILD=false
JSON_MODE=false
POS=()
for a in "$@"; do
	case "$a" in
		--positions) POSITIONS="--positions" ;;
		--skip-build) SKIP_BUILD=true ;;
		--json) JSON_MODE=true ;;
		*) POS+=("$a") ;;
	esac
done
usage() {
	echo "usage: compare-index.sh <inputFileOrDir|corpusJsonl> [numDocs] [--positions] [--skip-build] [--json]" >&2
	exit 2
}
[[ ${#POS[@]} -ge 1 ]] || usage
INPUT="${POS[0]}"
DOCS="${POS[1]:-}"

RUST_DIR="${RUST_DIR:-/tmp/compare-rust}"
JAVA_DIR="${JAVA_DIR:-/tmp/compare-java}"
JNI_DIR="${JNI_DIR:-/tmp/compare-jni}"
SAMPLE_PCT="${SAMPLE_PCT:-10}"
BATCH_SIZE="${BATCH_SIZE:-1000}"
COMPARE_SKIP_BUILD="${COMPARE_SKIP_BUILD:-0}"

if $PACKAGED; then
	RUSTLUCENE_BIN="$SCRIPT_DIR/bin/rustlucene-cli"
	JAVA_CLASSES="$SCRIPT_DIR/java/classes"
	JAVA_LIB="$SCRIPT_DIR/java/lib"
else
	RUSTLUCENE_BIN="$ROOT/target/release/rustlucene-cli"
	JAVA_CLASSES="$ROOT/interop/java/classes"
	JAVA_LIB="$ROOT/interop/java/lib"
fi

# JNI batch mode needs the cdylib on java.library.path.
JNI_LIB_PATH="$ROOT/target/release"
if $PACKAGED && [[ -f "$SCRIPT_DIR/bin/librustlucene_jni.so" ]]; then
	JNI_LIB_PATH="$SCRIPT_DIR/bin"
fi

CP="$JAVA_CLASSES:$JAVA_LIB/lucene-core-9.12.3.jar:$JAVA_LIB/lucene-analysis-common-9.12.3.jar"

# --- resolve time command --------------------------------------------------
# Prefer GNU time (supports -f) for full metrics; fall back to wall-clock via
# date(1) on systems without it (e.g. Huawei EulerOS).
run_timed() {
	local outfile="$1"; shift
	# Try GNU time (usually /usr/bin/time on Linux)
	if /usr/bin/time -f '%e' /bin/true &>/dev/null 2>&1; then
		/usr/bin/time -f '%e %U %S %M %P' -o "$outfile" "$@"
	else
		# fallback: wall-clock only, rest is zeroed
		local t0 t1 wall
		t0=$(date +%s%N 2>/dev/null || echo 0)
		"$@"
		local ret=$?
		t1=$(date +%s%N 2>/dev/null || echo 0)
		if [[ "$t0" != "0" && "$t1" != "0" ]]; then
			wall=$(awk "BEGIN { printf \"%.2f\", ($t1 - $t0) / 1000000000 }")
		else
			wall="0"
		fi
		echo "$wall 0 0 1 0" > "$outfile"  # RSS=1 avoids div-by-zero
		return $ret
	fi
}
GNU_TIME_OK=false
/usr/bin/time -f '%e' /bin/true &>/dev/null 2>&1 && GNU_TIME_OK=true

# --- build (skip when packaged or explicitly asked) -----------------------
if $PACKAGED; then
	echo "== packaged mode (build skipped)"
elif [[ "$COMPARE_SKIP_BUILD" == "1" ]] || $SKIP_BUILD; then
	echo "== build skipped (COMPARE_SKIP_BUILD=$COMPARE_SKIP_BUILD, --skip-build=$SKIP_BUILD)"
	if [[ ! -x "$RUSTLUCENE_BIN" ]]; then
		echo "ERROR: $RUSTLUCENE_BIN not found; run 'cargo build --release' first" >&2
		exit 1
	fi
	if [[ ! -d "$JAVA_CLASSES" ]] || [[ -z "$(ls -A "$JAVA_CLASSES" 2>/dev/null)" ]]; then
		echo "ERROR: $JAVA_CLASSES is empty; run 'make java-classes' first" >&2
		exit 1
	fi
else
	echo "== build"
	cd "$ROOT"
	cargo build -q --release -p rustlucene-core
	if $JSON_MODE; then
		cargo build -q --release -p rustlucene-jni
	fi
	mkdir -p "$JAVA_CLASSES"
	"$JAVAC" -cp "$CP" -d "$JAVA_CLASSES" "$ROOT/interop/java"/*.java
fi

# --- shared helpers ---------------------------------------------------------
get() { grep -oE "$2=[0-9]+" <<<"$1" | head -1 | cut -d= -f2; }

check_index() {
	local dir="$1"
	"$JAVA" -cp "$CP" org.apache.lucene.index.CheckIndex "$dir" 2>&1 | grep -E "No problems|FAILED" || true
	"$JAVA" -cp "$CP" org.apache.lucene.index.CheckIndex "$dir" > /dev/null 2>&1
}

# term-sample diff of A vs B on each given indexed field
compare_fields() {
	local a="$1" b="$2"; shift 2
	local f
	for f in "$@"; do
		"$JAVA" -cp "$CP" CompareIndexes "$a" "$b" "$f" "$SAMPLE_PCT" 42
	done
}

summary_header() {
	awk 'BEGIN { printf "%-10s %8s %10s %8s %9s %9s %6s %10s %12s %10s\n", \
		"writer", "docs", "elapsed_ms", "wall_s", "cpu_user", "cpu_sys", "cpu%", "maxrss_mb", "docs_per_sec", "dir_bytes" }'
}

# summary_row <name> <timeFile> <writerOutput> <indexDir>
summary_row() {
	local name="$1" timef="$2" out="$3" dir="$4"
	local wall user sys rss cpupct
	read -r wall user sys rss cpupct < "$timef"
	local ms dps docs bytes
	ms=$(get "$out" elapsed_ms); dps=$(get "$out" docs_per_sec); docs=$(get "$out" docs)
	bytes=$(du -sb "$dir" | cut -f1)
	awk -v n="$name" -v d="$docs" -v m="$ms" -v w="$wall" -v u="$user" -v s="$sys" \
	    -v p="$cpupct" -v r="$rss" -v dps="$dps" -v b="$bytes" \
	    'BEGIN { printf "%-10s %8d %10d %8.2f %9.2f %9.2f %6s %10.1f %12d %10d\n", n, d, m, w, u, s, p, r/1024, dps, b }'
}

# =============================================================================
if $JSON_MODE; then
	DOCS="${DOCS:-200000}"
	SEED=42
	# Schema matching JavaJsonIndex's document shape.
	SCHEMA="timestamp:longpoint+numericdv+stored,level:keyword+sorteddv,trace_id:keyword,message:text,latency_ms:intpoint+numericdv+stored"

	echo "== jsongen ($DOCS docs, seed $SEED) -> $INPUT"
	"$RUSTLUCENE_BIN" jsongen "$INPUT" "$DOCS" "$SEED"

	rm -rf "$RUST_DIR" "$JNI_DIR" "$JAVA_DIR"
	mkdir -p "$RUST_DIR" "$JNI_DIR" "$JAVA_DIR"

	echo "== Rust jsonindex"
	R_OUT=$(run_timed /tmp/compare-time-rust.txt \
	  "$RUSTLUCENE_BIN" jsonindex "$INPUT" "$RUST_DIR" "$SCHEMA")
	echo "$R_OUT"

	echo "== Java JNI batch (JsonJniBench, batch=$BATCH_SIZE)"
	N_OUT=$(run_timed /tmp/compare-time-jni.txt \
	  "$JAVA" -Djava.library.path="$JNI_LIB_PATH" -cp "$CP" \
	  JsonJniBench "$INPUT" "$JNI_DIR" "$SCHEMA" "$BATCH_SIZE")
	echo "$N_OUT"

	echo "== Java stock (JavaJsonIndex)"
	J_OUT=$(run_timed /tmp/compare-time-java.txt \
	  "$JAVA" -cp "$CP" JavaJsonIndex "$INPUT" "$JAVA_DIR")
	echo "$J_OUT"

	for side in rust:"$RUST_DIR" jni:"$JNI_DIR" java:"$JAVA_DIR"; do
		echo "== CheckIndex (${side%%:*})"
		check_index "${side#*:}"
	done

	echo "== SUMMARY"
	summary_header
	summary_row rust /tmp/compare-time-rust.txt "$R_OUT" "$RUST_DIR"
	summary_row java-jni /tmp/compare-time-jni.txt "$N_OUT" "$JNI_DIR"
	summary_row java /tmp/compare-time-java.txt "$J_OUT" "$JAVA_DIR"

	echo "== Compare term postings (random ${SAMPLE_PCT}% of dictionary)"
	for pair in "rust:$RUST_DIR" "jni:$JNI_DIR"; do
		echo "-- ${pair%%:*} vs java"
		compare_fields "${pair#*:}" "$JAVA_DIR" message level trace_id
	done

	echo "COMPARE_INDEXES_OK"
	exit 0
fi

# --- text mode (original pipeline) -------------------------------------------
FLAGS=()
[[ -n "$DOCS" ]] && FLAGS+=(--docs "$DOCS")
[[ "$POSITIONS" == "--positions" ]] && FLAGS+=(--positions)

rm -rf "$RUST_DIR" "$JAVA_DIR"
mkdir -p "$RUST_DIR" "$JAVA_DIR"

echo "== Rust write ($INPUT ${FLAGS[*]:-once})"
R_OUT=$(run_timed /tmp/compare-time-rust.txt \
  "$RUSTLUCENE_BIN" index "$INPUT" "$RUST_DIR" "${FLAGS[@]}")
echo "$R_OUT"

echo "== Java write ($INPUT ${FLAGS[*]:-once})"
J_OUT=$(run_timed /tmp/compare-time-java.txt \
  "$JAVA" -cp "$CP" JavaIndex "$INPUT" "$JAVA_DIR" "${FLAGS[@]}")
echo "$J_OUT"

read -r R_WALL R_USER R_SYS R_RSS R_CPUPCT < /tmp/compare-time-rust.txt
read -r J_WALL J_USER J_SYS J_RSS J_CPUPCT < /tmp/compare-time-java.txt
R_MS=$(get "$R_OUT" elapsed_ms); R_DPS=$(get "$R_OUT" docs_per_sec); R_DOCS=$(get "$R_OUT" docs)
J_MS=$(get "$J_OUT" elapsed_ms); J_DPS=$(get "$J_OUT" docs_per_sec); J_DOCS=$(get "$J_OUT" docs)
R_BYTES=$(du -sb "$RUST_DIR" | cut -f1)
J_BYTES=$(du -sb "$JAVA_DIR" | cut -f1)

echo "== CheckIndex (rust)"
check_index "$RUST_DIR"

echo "== CheckIndex (java)"
check_index "$JAVA_DIR"

echo "== SUMMARY"
awk -v rd="$R_DOCS"  -v rm="$R_MS"  -v rw="$R_WALL" -v ru="$R_USER" -v rs="$R_SYS" -v rr="$R_RSS" -v rp="$R_CPUPCT" -v rdps="$R_DPS" -v rb="$R_BYTES" \
    -v jd="$J_DOCS"  -v jm="$J_MS"  -v jw="$J_WALL" -v ju="$J_USER" -v js="$J_SYS" -v jr="$J_RSS" -v jp="$J_CPUPCT" -v jdps="$J_DPS" -v jb="$J_BYTES" '
BEGIN {
  printf "%-6s %8s %10s %8s %9s %9s %6s %10s %12s %10s\n", \
    "writer", "docs", "elapsed_ms", "wall_s", "cpu_user", "cpu_sys", "cpu%", "maxrss_mb", "docs_per_sec", "dir_bytes"
  printf "%-6s %8d %10d %8.2f %9.2f %9.2f %6s %10.1f %12d %10d\n", "rust", rd, rm, rw, ru, rs, rp, rr/1024, rdps, rb
  printf "%-6s %8d %10d %8.2f %9.2f %9.2f %6s %10.1f %12d %10d\n", "java", jd, jm, jw, ju, js, jp, jr/1024, jdps, jb
  speedup_wall = (rw > 0 && jw > 0) ? jw/rw : 0
  speedup_cpu  = ((ru+rs) > 0 && (ju+js) > 0) ? (ju+js)/(ru+rs) : 0
  speedup_rss  = (rr > 1 && jr > 1) ? jr/rr : 0
  printf "speedup: throughput %.2fx | wall %.2fx | cpu_total %.2fx | rss java/rust %.2fx | dir_size java/rust %.2fx\n", \
    rdps/jdps, speedup_wall, speedup_cpu, speedup_rss, jb/rb
}'

echo "== Compare term postings (random ${SAMPLE_PCT}% of dictionary)"
compare_fields "$RUST_DIR" "$JAVA_DIR" message

echo "COMPARE_INDEXES_OK"
