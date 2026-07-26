# Task 2 Review: codec 窗口批读——EnumCore::next_docs

**Reviewer:** Task 2 reviewer  
**Commit:** 50b3e1b  
**Date:** 2026-07-26

---

## Verdict 1: Spec Compliance

### Step-by-step checklist

| Step | Requirement | Status |
|------|-------------|--------|
| 1 | Write failing test (codec batch parity) | ✅ Two tests added: `next_docs_matches_next_doc_all_terms`, `no_freq_enum_does_not_decode_freqs` |
| 2 | Confirm test fails (compile error) | ✅ RED output shows 3 missing methods |
| 3 | `EnumCore::next_docs` implementation | ✅ Verbatim transcription — line-by-line match with brief Step 3 code |
| 4 | `DocsEnum::next_docs`, `DocsFreqsEnum::{next_docs, next_docs_and_freqs, decodes_freqs}` | ✅ All four wrappers present; `decodes_freqs` pre-existing from Task 1 (correctly not duplicated) |
| 5 | Run tests, confirm pass | ✅ 279 passed (187 codec + 89 core + 3 jni/other) |
| 6 | Single commit, Chinese subject, Co-Authored-By | ✅ Commit message matches brief verbatim |

### Naming contract

| Interface | Brief | Actual | Match |
|-----------|-------|--------|-------|
| `EnumCore::next_docs` | `fn next_docs(&mut self, docs: &mut [u32], freqs: Option<&mut [u32]>) -> io::Result<usize>` | Identical (with `mut` on binding) | ✅ |
| `DocsEnum::next_docs` | `pub fn next_docs(&mut self, docs: &mut [u32]) -> io::Result<usize>` | Identical | ✅ |
| `DocsFreqsEnum::next_docs` | `pub fn next_docs(&mut self, docs: &mut [u32]) -> io::Result<usize>` | Identical | ✅ |
| `DocsFreqsEnum::next_docs_and_freqs` | `pub fn next_docs_and_freqs(&mut self, docs: &mut [u32], freqs: &mut [u32]) -> io::Result<usize>` | Identical | ✅ |
| `DocsFreqsEnum::decodes_freqs` | `pub fn decodes_freqs(&self) -> bool` | Identical (pre-existing) | ✅ |

### Constraint 1: per-doc path untouched

`git diff 2435f5d..50b3e1b` contains **zero deletion lines**. All 200 insertions are purely additive. The existing `next_doc()` at :292-309 and all per-doc infrastructure are byte-identical. ✅

### Nothing added beyond the brief

Production code (EnumCore::next_docs, DocsEnum::next_docs, DocsFreqsEnum wrappers) is a verbatim transcription of the brief's Step 3/4 code. Test code has the adjudicated adjustment only (see below). No extra methods, no extra helpers, no refactoring. ✅

### Test deviation (adjudicated)

**Ruling verification:** The brief's test used `reader.docs()` for `tx` field terms (DOCS_AND_FREQS). `reader.docs()` builds an EnumCore with `has_freqs=false`, which cannot skip on-disk PFOR freq blocks — this corrupts even per-doc `next_doc()` on those terms. The implementer correctly identified this and split:
- `kw` (DOCS-only) → `reader.docs()` + `DocsEnum::next_docs`
- `tx` (DOCS_AND_FREQS) → `reader.docs_and_freqs_no_freq()` + `DocsFreqsEnum::next_docs` for docs parity
- `tx` freq path → `reader.docs_and_freqs()` + `DocsFreqsEnum::next_docs_and_freqs` for freq parity

**Coverage preserved.** The adjusted tests still exercise:
- All 5 terms (big/tail/hot/warm/one) across all 5 dst sizes (1/7/128/200/4096): ✅ 25 batch-vs-expected comparisons
- Full-stream equality (Vec comparison via `assert_eq!`, not count-only): ✅
- Freq values on the freq-decoding path: ✅ (3 terms × `next_docs_and_freqs` drain)
- `decodes_freqs()` positive and negative: ✅
- No-freq batch read: ✅ (`no_freq_enum_does_not_decode_freqs`)

The deviation is bounded, correct, and preserves the brief's intent. Ruling **upheld**.

---

## Verdict 2: Code Quality

### EnumCore::next_docs — correctness analysis

**Window-copy loop (lines 320-353):**

The main loop copies a window of decoded absolute doc IDs from `doc_buffer[upto..upto+take]` into the caller's `docs` slice. Three invariants hold:

1. **Sentinel termination.** `doc_buffer` is `[u64; BLOCK_SIZE+1]` (129 slots). `doc_buffer[128]` is permanently set to `NO_MORE_DOCS as u64` during `new()` (:287) and is never overwritten by `refill_full_block` (which writes `doc_buffer[..128]` only). `refill_remainder` writes its own sentinel at `doc_buffer[left]` where `left ≤ 127`. So the inner `take` loop always terminates before indexing out of bounds. ✅

2. **Cross-block refill.** After consuming all docs in a block (take loop exits because `doc_buffer[upto+take]` is the sentinel), `doc_buffer_upto` is set to the sentinel position. On the next outer iteration, `self.doc == self.level0_last_doc` triggers `move_to_next_level0_block()`, which refills and resets `doc_buffer_upto = 0`. The loop continues transparently. ✅

3. **take == 0 sentinel edge.** This fires when the buffer's first slot is the sentinel — specifically when `doc_buffer_upto` points at the sentinel (e.g., after consuming an exact-128-multiple df's last full block, the subsequent refill_remainder(0) puts a sentinel at slot 0). The handler mirrors `next_doc`'s sentinel read: `self.doc = NO_MORE_DOCS`, `doc_buffer_upto += 1`. After this, the `doc == NO_MORE_DOCS` guard at the top of the loop terminates cleanly. ✅

**df exact-multiple-of-128 path (deep trace):**

Consider df=256 (two full blocks, no remainder):
- Block 1: take=128, `doc_buffer_upto=128`, `doc=doc_buffer[127]=level0_last_doc`. ✅
- Block 2: `move_to_next_level0_block` called, `refill_full_block`, `doc_buffer_upto=0`, take=128, `doc_buffer_upto=128`, `doc=level0_last_doc`. ✅
- Block 3 attempt: `move_to_next_level0_block`, `doc_freq - doc_count_upto = 0 < 128`, `level0_last_doc = NO_MORE_DOCS`, `refill_remainder(0)`: `doc_buffer[0] = NO_MORE_DOCS`, `doc_buffer_upto = 0`.
- Back in next_docs: take=0 (sentinel at slot 0), `self.doc = NO_MORE_DOCS`, `doc_buffer_upto = 1`, break. Return total. ✅
- After break: `doc_buffer_upto = 1` (out of meaningful data, but `doc = NO_MORE_DOCS` ensures `next_doc`/`next_docs` early-returns without touching the buffer). ✅

**Test coverage of this path:** `tx:hot` (df=5000) exercises 39 full-block transitions (4992/128) plus remainder(8), and with step=200 and step=4096 the `take==0` sentinel branch is hit during the final drain iteration. ✅

**Freq-buffer alignment (lines 345-349):**

`freq_buffer` has the same indexing as `doc_buffer` (`[u32; BLOCK_SIZE]`). The `for j in 0..take` loop copies `freq_buffer[upto + j]` for the same `(upto, take)` used for docs. Since `take ≤ 128 - upto` (bounded by the sentinel at position ≤ 128), and `freq_buffer` has 128 slots, `upto + j < 128`. ✅

Note: `freq_buffer` is not populated when `decode_freqs = false` (refill_full_block skips PFOR bytes, refill_remainder discards freq vints). However, the public API prevents reading stale freq data: `DocsFreqsEnum::next_docs_and_freqs` asserts `self.core.decode_freqs` before passing `Some(freqs)`, and `DocsFreqsEnum::next_docs` / `DocsEnum::next_docs` pass `None`. ✅

**debug_assert on positions profile (line 318):**

`debug_assert!(self.pos.is_none())` correctly restricts batch reads to non-positions profiles. EverythingEnum (which has `pos.is_some()`) is built via `new_with_positions` and is never exposed through DocsEnum or DocsFreqsEnum. ✅

**assert in next_docs_and_freqs (line 767):**

`assert!(self.core.decode_freqs, "next_docs_and_freqs on no-freq enum")` matches the brief and mirrors `freq()`'s no-freq panic contract (:628-631). ✅

**Partial-consumption state consistency:**

When the outer loop exits because `n == docs.len()` (dst filled):
- If last iteration had take>0: `doc = doc_buffer[upto+take-1]`, `doc_buffer_upto = upto+take`. Subsequent `next_doc()` or `next_docs()` calls correctly resume from this position. ✅
- Interleaving `next_docs` and `next_doc` on the same enum: both methods share the same state machine (`doc`, `doc_buffer_upto`, `level0_last_doc`) and the same sentinel/refill logic. State transitions are compatible. ✅

### Production code findings

No Critical or Important issues found.

**Minor findings:**

1. **Minor — Imperative copy loops (lines 342-348).** The `for j in 0..take` loops could be expressed as `docs[n..n+take].copy_from_slice(...)` and `f[n..n+take].copy_from_slice(...)` for idiomatic Rust and potential compiler auto-vectorization. The current form is correct and compiles to equivalent code, but the slice form is clearer about intent. Not blocking.

### Test quality analysis

**Full-stream equality, not just counts:** ✅

- `assert_eq!(drain_next_docs(&mut en, step), expect_docs, ...)` — compares full `Vec<u32>`. ✅
- `assert_eq!(docs, expect_docs, ...)` and `assert_eq!(freqs, expect_f, ...)` — full freq parity. ✅
- `assert_eq!(drain_per_doc(&mut en), expect_docs)` — per-doc reference also full-vector. ✅

**Coverage matrix:**

| Term | df | field type | dst sizes | docs parity | freq parity |
|------|-----|-----------|-----------|-------------|-------------|
| kw:big | 200 | DOCS | 1/7/128/200/4096 | ✅ | n/a |
| kw:tail | 3 | DOCS | 1/7/128/200/4096 | ✅ | n/a |
| tx:hot | 5000 | DOCS_AND_FREQS | 1/7/128/200/4096 | ✅ | ✅ |
| tx:warm | 200 | DOCS_AND_FREQS | 1/7/128/200/4096 | ✅ | ✅ |
| tx:one | 1 | DOCS_AND_FREQS | 1/7/128/200/4096 | ✅ | ✅ |

**Edge cases covered:** singleton (tx:one), small remainder block (kw:tail, df=3), cross level-1 boundary (tx:hot, df=5000 > 4096=LEVEL1_NUM_DOCS), freq outliers (tx:warm), dst=1 degenerate, dst=4096 super-level-1. ✅

**Coverage gap (Minor):**

2. **Minor — No df=exact-128 test term.** No term has df exactly equal to 128 or 256 (a multiple of BLOCK_SIZE). The `take==0` sentinel branch is exercised indirectly via cross-block exhaustion (e.g., step=4096 on df=5000 eventually triggers it), but an explicit df=128 term would provide more direct coverage of this edge case. Not blocking — the branch is reachable and correct via existing tests.

---

## Summary

### Spec compliance: ✅

All 6 steps present. Naming contract exact. Production code is verbatim brief transcription. Test deviation is the adjudicated fix, correctly bounded. No existing per-doc code modified (zero deletion lines in diff). Single commit, correct message.

### Code quality: Approved

**Critical: 0 | Important: 0 | Minor: 2**

- Minor: Imperative copy loops could use slice copy_from_slice (cosmetic)
- Minor: No explicit df=128 test term (take==0 branch covered indirectly)

### Adjudicated test deviation: coverage preserved

The split between `reader.docs()` (DOCS fields) and `reader.docs_and_freqs_no_freq()` (DOCS_AND_FREQS fields) preserves all 25 batch-vs-reference comparisons and the full freq parity check. No coverage was lost.

### Cannot-verify items: none

All aspects verified from the diff, code reading, and report evidence.
