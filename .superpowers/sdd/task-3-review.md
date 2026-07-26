# Task 3 Review: 叶子覆写——MatchAll / Bitmap 游标 / Docs / Freqs

**Reviewer**: task-3-reviewer
**Commit**: `0bb726d` feat(batch): 叶子 next_block 覆写——bitmap 游标 next_many_to / MatchAll 算术填充 / Docs-Freqs codec 批读
**Diff**: 50b3e1b..0bb726d (2 files, +101 -22)

---

## Verdict 1: Spec compliance — ✅

### All 8 steps present

| Step | Requirement | Status |
|------|------------|--------|
| 1 | Tests: `matchall_block_stream` + `leaves_block_vs_per_doc_random` | ✅ block_tests.rs:137–157 |
| 2 | Baseline confirmed PASS before overrides | ✅ (reported in §Self-Review) |
| 3 | `BitmapCursor::next_many_to` | ✅ doc_iter.rs:749–772 |
| 4 | Shared `cursor_next_block<B>` + one-line call sites | ✅ doc_iter.rs:777–790, call sites at :836–838 and :888–890 |
| 5 | `MatchAllIter::next_block` arithmetic fill | ✅ doc_iter.rs:122–139 |
| 6 | Docs/Freqs arms → codec batch | ✅ doc_iter.rs:1703–1716 |
| 7 | Full suite green (281 passed) | ✅ reported, not re-run per instructions |
| 8 | Single commit, Chinese subject, Co-Authored-By | ✅ verified |

### Naming contract

- `BitmapCursor::next_many_to(&mut self, dst: &mut [u32]) -> usize` ✅
- `cursor_next_block<B: DocsBitmap>(cur, doc, out) -> io::Result<usize>` ✅
- Consumes `DocsEnum::next_docs`, `DocsFreqsEnum::{next_docs, next_docs_and_freqs, decodes_freqs}` ✅

### Production code matches brief modulo adjudication 1

All code matches the brief's specifications with the single documented deviation (adjudication 1: mid-loop `next_from` update in `next_many_to`). No extraneous changes.

### Constraint 1: per-doc path unchanged

**Confirmed.** The diff shows:
- `next_doc()` dispatch in SegmentDocIter (lines 1649–1665): **zero changes**
- `advance()` dispatch in SegmentDocIter (lines 1667–1683): **zero changes**
- `doc_id()` dispatch (lines 1631–1647): **zero changes**
- `freq()` dispatch (lines 1685–1691): **zero changes**
- `matches()` dispatch (lines 1693–1697): **zero changes**

The only changes to SegmentDocIter are within the `next_block` method — the Docs/Freqs arms replaced inline per-doc fill with codec batch calls. The per-doc code path is line-for-line identical.

### Adjudicated deviations verified

1. **BitsetDocIter stays default fill** ✅ — no override added, `Self::Bitset(b) => b.next_block(out)` dispatches to trait default
2. **PhraseDocIter stays per-doc** ✅ — no override added
3. **Shared helper** ✅ — `cursor_next_block` is a single generic free function called by both RoaringDocIter and MaterializedDocIter

---

## Verdict 2: Code quality — Approved

**Critical: 0 | Important: 0 | Minor: 1**

### MatchAllIter::next_block arithmetic (doc_iter.rs:122–139)

**Correctness verified across all edge cases:**

| Scenario | Trace | Result |
|----------|-------|--------|
| doc=-1 (initial) | start = (-1+1).max(0) = 0 | ✅ First doc is 0 |
| max_doc=0 | n = 0.saturating_sub(0).min(128) = 0 → doc = NO_MORE_DOCS | ✅ Returns 0 |
| max_doc=128 (exact DOC_BLOCK) | n=128, start+n=128 >= 128 → doc=NO_MORE_DOCS | ✅ Single block [0..127] |
| max_doc=129 | Block 1: [0..127], doc=127. Block 2: start=128, n=1, 128+1=129>=129 → NO_MORE | ✅ [0..128] in 2 blocks |
| max_doc=300 | 3 blocks: [0..127], [128..255], [256..299] | ✅ |
| max_doc=1000 | 8 blocks, last partial | ✅ |
| NO_MORE_DOCS pinning | Early return with out.len=0 | ✅ Idempotent |

**u32 overflow**: `self.doc` is i32 with `doc < max_doc <= i32::MAX` when not exhausted, so `doc + 1 <= i32::MAX`, and the `as u32` cast is safe. `start + n as u32 <= max_doc + 128` fits in u32. No overflow risk.

**Mixed advance/next_block**: After `advance(target)` returns target, next_block computes `start = target + 1`, correctly producing docs AFTER the current position. ✅

### cursor_next_block doc-cursor bookkeeping (doc_iter.rs:777–790)

**State machine consistency verified:**

- **Exhaustion**: `n == 0` → doc pinned to NO_MORE_DOCS. Subsequent calls early-return. ✅
- **After successful block**: doc = `out.docs[n-1] as i32`. The cursor's `next_from` is `out.docs[n-1] + 1` (set by next_many_to's final update), so the next block starts from the doc AFTER the last emitted. ✅
- **After advance(target)**: advance sets `cur.next_from = d + 1` (via the internal `next()` call) and `doc = d as i32`. next_block's next_many_to starts from `next_from = d + 1`, emitting docs AFTER d. ✅
- **Mixed advance/next_block sequences**: Both paths maintain the invariant `doc = last_emitted_doc` and `next_from = last_emitted_doc + 1`. Consistent. ✅

**u32→i32 cast safety**: `out.docs[n-1]` is a doc ID < max_doc <= i32::MAX, so `as i32` produces a non-negative value. Same pattern as `next()` in existing code. ✅

### BitmapCursor::next_many_to — adjudication 1 fix (doc_iter.rs:749–772)

**Verdict: The fix is correct and the invariant holds across all scenarios.**

**Scenario analysis:**

1. **No refill needed** (buffer has enough docs for dst): Loop fills dst without entering the `pos >= end` branch. Final update `next_from = dst[n-1] + 1`. ✅

2. **Single refill** (buffer exhausted, one refill suffices): Buffer drains → pos >= end → mid-loop update `next_from = dst[k-1] + 1` → refill() calls `docs_from(next_from, buf)` → continues filling. Final update overwrites with `dst[n-1] + 1`. The mid-loop value is consumed by refill() and then superseded. ✅

3. **Multiple refills** (dst.len() > BITMAP_ITER_BATCH): Each refill boundary triggers a mid-loop update, ensuring `docs_from` seeks past all previously emitted docs. Without the fix, `docs_from` would use the stale `next_from` from before the call, re-fetching overlapping docs → duplicates. **The fix is load-bearing for correctness**, not merely defensive. ✅

4. **Exhaustion mid-fill**: refill() returns false → loop breaks. Final update sets `next_from = dst[n-1] + 1`. Next call: pos >= end, n=0, skip mid-loop update (guard `n > 0`), refill returns false (exhausted=true remains), break, return 0. ✅

**No double-update hazard**: The mid-loop update is a prerequisite for refill(); the final update unconditionally sets the correct post-call invariant. They serve different purposes and don't conflict.

**No off-by-one**: `dst[n-1] + 1` — since docs are ascending u32 values < max_doc <= i32::MAX ≈ 2.1 billion, adding 1 yields at most ~2.1 billion which fits in u32 (max ~4.3 billion). ✅

**Current deployment**: DOC_BLOCK=128 <= BITMAP_ITER_BATCH=512, so a single buffer always suffices and the mid-loop path never fires in practice today. But the fix is correct for future callers and maintains the documented invariant.

### Docs arm (doc_iter.rs:1703–1706)

`d.next_docs(&mut out.docs)` fills docs, returns count. No freqs written — `out.freqs` retains prior values. This matches the old behavior (the old inline loop also skipped freqs for Docs). The driver never reads freqs from a Docs-only iterator. ✅

### Freqs arm (doc_iter.rs:1708–1716)

- `decodes_freqs() == true` → `next_docs_and_freqs(docs, freqs)`: fills both in lockstep. Same doc/freq alignment as old per-doc `next_doc()` + `freq()`. ✅
- `decodes_freqs() == false` → `next_docs(docs)`: fills docs only. Freqs left at prior values. The driver gates on `needs_freq` — when `needs_freq` is true, the enum was constructed with `decode_freqs=true`, so this branch is only reached when freqs are not needed. Same behavior as old code. ✅

**Codec batch semantics**: Both `next_docs` and `next_docs_and_freqs` return 0 on exhaustion (verified in postings_read.rs:317–355), transparently cross 128-doc block boundaries via `move_to_next_level0_block()`, and maintain ascending absolute doc IDs. The batch call is a drop-in replacement for the old per-doc loop.

### Parity tests (block_tests.rs:137–157)

- `matchall_block_stream`: Tests max_doc ∈ {0, 1, 127, 128, 129, 300, 1000}. Asserts **full-stream doc list equality** (not counts). Covers zero-length, exact-DOC_BLOCK, partial-block, and multi-block scenarios. ✅
- `leaves_block_vs_per_doc_random`: 20 random Materialized doc sets (n < 600, universe 50K). Asserts `stream_block(a) == stream_per_doc(b)` — full-stream doc equality. ✅
- Both tests exercise the new overrides (MatchAll via `SegmentDocIter::All`, Materialized via `SegmentDocIter::Materialized`). ✅

---

## Minor findings

### Minor-1: Roaring path not directly tested in this task (informational)

`RoaringDocIter` requires a `FrozenBitmap` (codec-internal view), which is hard to construct in unit tests. The brief explicitly defers Roaring path validation to Task 8's 1M battery. The `cursor_next_block` shared helper is generic over `B: DocsBitmap`, so the Materialized parity test covers the cursor logic. The Roaring-specific `docs_from` is tested by the codec test suite. **No action needed** — this is a documented and accepted gap.

---

## Summary

| Category | Verdict |
|----------|---------|
| **Spec compliance** | ✅ All 8 steps, naming contract, commit discipline, constraint 1 (per-doc unchanged) |
| **Code quality** | Approved — Critical: 0, Important: 0, Minor: 1 (informational) |
| **Adjudication 1** | Fix is correct. Invariant holds across all mid-loop refill scenarios. No double-update, no off-by-one. |
| **Cannot-verify items** | None — all code paths analyzed against the diff and surrounding source. |
