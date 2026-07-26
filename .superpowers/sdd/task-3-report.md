# Task 3 Report: 叶子覆写——MatchAll / Bitmap 游标 / Docs / Freqs

## Status: DONE_WITH_CONCERNS

## Commits
- `0bb726d` feat(batch): 叶子 next_block 覆写——bitmap 游标 next_many_to / MatchAll 算术填充 / Docs-Freqs codec 批读

## Test Summary
- 281 passed (187 codec + 91 core + 2 bin + 1 jni); +2 vs baseline 279 (new leaf parity tests); 0 failed; 0 warnings.

## Files Changed
- `crates/core/src/search/doc_iter.rs` — 4 edits: `BitmapCursor::next_many_to`, `cursor_next_block` free fn, `RoaringDocIter::next_block`, `MaterializedDocIter::next_block`, `MatchAllIter::next_block`, `SegmentDocIter::next_block` Docs/Freqs arms
- `crates/core/src/search/block_tests.rs` — added `matchall_block_stream` and `leaves_block_vs_per_doc_random` tests; added `MatchAllIter` import

## Self-Review

### Completeness (all 8 steps)
- [x] Step 1: Tests appended to block_tests.rs
- [x] Step 2: Baseline confirmed 6/6 block tests pass before overrides (regression guard, not red-light)
- [x] Step 3: `BitmapCursor::next_many_to` added after `advance` in impl block
- [x] Step 4: `cursor_next_block<B: DocsBitmap>` free function + one-line call sites in both Roaring and Materialized impls
- [x] Step 5: `MatchAllIter::next_block` arithmetic fill
- [x] Step 6: Docs/Freqs arms replaced with codec batch calls
- [x] Step 7: Full workspace suite green
- [x] Step 8: Committed with exact message from brief

### Naming contract
- `next_many_to` ✓
- `cursor_next_block` ✓
- `next_docs` / `next_docs_and_freqs` consumption ✓

### Discipline
- Nothing beyond brief (one minor fix to brief code — see Drift)

### Pristine output
- No warnings

## Drift / Concerns

### Brief bug fix: `next_many_to` refill invariant

The brief's Step 3 code updates `next_from` only at the **end** of `next_many_to`, after the full while-loop. This creates a correctness bug: when `dst.len() > BITMAP_ITER_BATCH` (here 128 < 512 so not triggered in practice for DOC_BLOCK, but the invariant is wrong), the inner `refill()` call uses `self.next_from` which is stale — it still holds the value from the previous batch's last doc + 1, not from what was just consumed from `self.buf`. This causes `docs_from` to re-fetch overlapping docs, producing duplicates.

**Fix applied**: update `self.next_from = dst[n - 1] + 1` *before* calling `refill()` inside the loop (when `n > 0` and buffer is exhausted), in addition to the final update after the loop. The final update remains for the common case where the loop exits without refilling.

In the current codebase `dst.len() == DOC_BLOCK == 128 <= 512 == BITMAP_ITER_BATCH`, so a single buffer always suffices and the mid-loop refill never fires. The fix is defensive — corrects the invariant for future callers with larger dst.

### §12 Design deviation (as specified in brief)
`BitsetDocIter` retains the default `next_block` fill. `FixedBitSet` exposes no word-level access; `next_set_bit` is already a word-level trailing_zeros scan. A specialized override would yield no incremental benefit. If Phase 1 profiling confirms bitset-path hotspots, revisit.
