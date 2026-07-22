#!/usr/bin/env bash
# compare-search: benchmark search performance on two Lucene indexes
# (typically built by Rust vs Java writers from the same corpus).
#
# Methodology (matches luceneutil's per-JVM isolation):
#   1. Extract query terms from index A ('--dump-queries')
#   2. Run SearchBench on index A in a FRESH JVM → results_a.txt
#   3. Run SearchBench on index B in a FRESH JVM → results_b.txt
#   4. Print side-by-side comparison table
#
# Usage: interop/compare-search.sh <indexDirA> <indexDirB> <field> [--warmup N] [--iter N] [--tasks N]
#        interop/compare-search.sh <indexDirA> <indexDirB> <field> --from-corpus <corpusFile> [--docs N] [--positions]
#
# With --from-corpus: rebuilds BOTH indexes from the same corpus file, then
# compares. Useful when the input indexes may be stale.
#
# Env: JAVA_HOME, TASKS (default 50), WARMUP (default 10), ITER (default 30)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# --- resolve java ---------------------------------------------------------
if [[ -n "${JAVA_HOME:-}" ]]; then
  JAVA="$JAVA_HOME/bin/java"
else
  JAVA="java"
fi

# --- params ----------------------------------------------------------------
FROM_CORPUS=""
POSITIONS=""
DOCS=""
POS=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --from-corpus) FROM_CORPUS="$2"; shift 2 ;;
    --docs)        DOCS="$2"; shift 2 ;;
    --positions)   POSITIONS="--positions"; shift ;;
    --warmup)      WARMUP="$2"; shift 2 ;;
    --iter)        ITER="$2"; shift 2 ;;
    --tasks)       TASKS="$2"; shift 2 ;;
    *)             POS+=("$1"); shift ;;
  esac
done

TASKS="${TASKS:-50}"
WARMUP="${WARMUP:-10}"
ITER="${ITER:-30}"
SEED=42

usage() {
  echo "usage: compare-search.sh <indexDirA> <indexDirB> <field> [--warmup N] [--iter N] [--tasks N]" >&2
  echo "       compare-search.sh <indexDirA> <indexDirB> <field> --from-corpus <file> [--docs N] [--positions]" >&2
  exit 2
}

JAVA_CP="$ROOT/interop/java/classes:$ROOT/interop/java/lib/lucene-core-9.12.3.jar:$ROOT/interop/java/lib/lucene-analysis-common-9.12.3.jar"

# --- rebuild from corpus if requested --------------------------------------
if [[ -n "$FROM_CORPUS" ]]; then
  # Rebuild both indexes using the same corpus
  mkdir -p "${POS[0]}" "${POS[1]}"
  rm -rf "${POS[0]}"/* "${POS[1]}"/*
  echo "== Rebuilding Rust index: $FROM_CORPUS -> ${POS[0]}"
  "$ROOT/target/release/rustlucene-cli" index "$FROM_CORPUS" "${POS[0]}" $POSITIONS --docs "${DOCS:-100000}"
  echo "== Rebuilding Java index: $FROM_CORPUS -> ${POS[1]}"
  "$JAVA" -cp "$JAVA_CP" JavaIndex "$FROM_CORPUS" "${POS[1]}" $POSITIONS --docs "${DOCS:-100000}"
fi

DIR_A="${POS[0]}"
DIR_B="${POS[1]}"
FIELD="${POS[2]:-message}"

if [[ ! -d "$DIR_A" ]] || [[ ! -d "$DIR_B" ]]; then
  echo "ERROR: both index directories must exist (A=$DIR_A B=$DIR_B)" >&2
  exit 2
fi

# --- step 1: dump query terms from index A ---------------------------------
QUERY_FILE="$(mktemp -t search-queries-XXXXXX.txt)"
echo "== Dumping query terms from $DIR_A (field=$FIELD) ..."
"$JAVA" -cp "$JAVA_CP" SearchBench "$DIR_A" "$FIELD" \
  --dump-queries "$QUERY_FILE" --tasks "$TASKS" --seed "$SEED" 2>/dev/null
echo "   $(wc -l < "$QUERY_FILE") terms dumped"

# --- step 2 & 3: benchmark each index in a fresh JVM -----------------------
bench_one() {
  local label="$1" dir="$2"
  echo "== Benchmarking $label ..."
  "$JAVA" -cp "$JAVA_CP" SearchBench "$dir" "$FIELD" \
    --load-queries "$QUERY_FILE" --tasks "$TASKS" \
    --warmup "$WARMUP" --iter "$ITER" --seed "$SEED" 2>/dev/null
}

RESULT_A="$(mktemp -t search-result-a-XXXXXX.txt)"
RESULT_B="$(mktemp -t search-result-b-XXXXXX.txt)"

bench_one "A ($DIR_A)" "$DIR_A" > "$RESULT_A"
bench_one "B ($DIR_B)" "$DIR_B" > "$RESULT_B"

# --- step 4: compare --------------------------------------------------------
echo ""
echo "============================================================"
echo "  SEARCH PERFORMANCE COMPARISON"
echo "  tasks=$TASKS  warmup=$WARMUP  iterations=$ITER"
echo "============================================================"
echo ""

# Parse results and join (using Python for reliable parsing)
python3 << 'PYEOF'
import csv, sys

def load(fn):
    result = {}
    with open(fn) as f:
        for row in csv.DictReader(f, delimiter='\t'):
            key = (row['query_type'], row['freq'])
            result[key] = {k: float(row[k]) for k in ['qps','p50_us','p90_us','p99_us']}
    return result

rust = load(sys.argv[1])
java = load(sys.argv[2])

hdr = f"{'query_type':<10} {'freq':<8} {'qps_rust':>10} {'qps_java':>10} {'ratio':>10} {'r_p50':>8} {'j_p50':>8} {'r_p90':>8} {'j_p99':>8}"
print(hdr)
print("-" * len(hdr))

for key in rust:
    if key not in java:
        continue
    r, j = rust[key], java[key]
    ratio = r['qps'] / j['qps'] if j['qps'] > 0 else 0
    marker = " ***" if abs(ratio - 1.0) > 0.10 else ""
    print(f"{key[0]:<10} {key[1]:<8} {r['qps']:>10.1f} {j['qps']:>10.1f} {ratio:>9.3f}x {r['p50_us']:>8.1f} {j['p50_us']:>8.1f} {r['p90_us']:>8.1f} {j['p99_us']:>8.1f}{marker}")

ratios = [rust[k]['qps'] / java[k]['qps'] for k in rust if k in java and java[k]['qps'] > 0]
if ratios:
    avg = sum(ratios) / len(ratios)
    print(f"\nAverage: {avg:.3f}x  Min: {min(ratios):.3f}x  Max: {max(ratios):.3f}x")
PYEOF "$RESULT_A" "$RESULT_B"

echo ""
echo "compare-search OK"
rm -f "$QUERY_FILE" "$RESULT_A" "$RESULT_B"
