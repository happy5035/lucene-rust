# Task 2 Report: codec 窗口批读——EnumCore::next_docs

## What was implemented

### EnumCore::next_docs (private method)
- Batch-reads up to `docs.len()` documents from the decoded `doc_buffer` window
- Cross-block-boundary transparent refill via `move_to_next_level0_block()`
- When `freqs = Some(...)`, simultaneously copies the same window from `freq_buffer`
- Returns 0 when exhausted
- Handles sentinel edge cases (buffer-first-slot-is-sentinel after exact 128-multiple df)
- `debug_assert!(self.pos.is_none())` — positions profile stays per-doc

### DocsEnum::next_docs (public wrapper)
- Delegates to `self.core.next_docs(docs, None)`
- 0 = exhausted

### DocsFreqsEnum (public wrappers)
- `next_docs(&mut self, docs)` — doc-only batch (no freq decode needed)
- `next_docs_and_freqs(&mut self, docs, freqs)` — asserts `decode_freqs` then delegates
- `decodes_freqs(&self)` — **pre-existing** (added in Task 1 fix), verified identical to brief's code, not re-added

### Tests added
- `next_docs_matches_next_doc_all_terms` — 5 terms × 5 dst sizes (1/7/128/200/4096), batch vs per-doc full cross-check, plus freq round-trip
- `no_freq_enum_does_not_decode_freqs` — verifies no-freq mode's `decodes_freqs()` returns false and batch read still works

## Drift / adjustments from the brief

**One adjustment to the test code.** The brief's test used `reader.docs(&entry)` for all 5 terms including `tx` field terms (which have `IndexOptions::DocsAndFreqs`). However, `reader.docs()` constructs an `EnumCore` with `has_freqs=false`, which fails to skip on-disk freq PFOR blocks — the per-doc `next_doc()` itself produces corrupt output (doc 255 repeated 16 times, then 256 repeated 16 times, etc.) on `tx` terms. This is a pre-existing issue in the `docs()` constructor, not a bug in `next_docs`.

**Fix:** Split the test into two loops:
- `kw` field terms (DOCS-only) → `reader.docs()` + `drain_next_docs` (DocsEnum)
- `tx` field terms (DOCS_AND_FREQS) → `reader.docs_and_freqs_no_freq()` + `drain_next_docs_enum` (DocsFreqsEnum)

Added a `drain_per_doc_freqs` helper mirroring `drain_per_doc` for `DocsFreqsEnum`.

The production code (EnumCore::next_docs, DocsEnum::next_docs, DocsFreqsEnum wrappers) was transcribed exactly as specified — no drift.

## RED output (Step 2)

```
error[E0599]: no method named `next_docs` found for mutable reference `&mut postings_read::DocsEnum`
error[E0599]: no method named `next_docs_and_freqs` found for struct `postings_read::DocsFreqsEnum`
error[E0599]: no method named `next_docs` found for mutable reference `&mut postings_read::DocsFreqsEnum`
error: could not compile `codec-lucene9` (lib test) due to 3 previous errors
```

As predicted in the pre-resolved ambiguity: `decodes_freqs` compiled fine (pre-existing), only `next_docs` and `next_docs_and_freqs` were missing.

## GREEN output (Step 5)

### Focused tests
```
test postings_read::tests::next_docs_matches_next_doc_all_terms ... ok
test postings_read::tests::no_freq_enum_does_not_decode_freqs ... ok
```

### Full workspace suite
```
codec-lucene9:  187 passed; 0 failed; 1 ignored
rustlucene-core: 89 passed; 0 failed; 1 ignored
rustlucene-jni:  2 passed; 0 failed; 0 ignored
                 1 passed; 0 failed; 0 ignored (binary)
Total: 279 passed (baseline 277 + 2 new)
```

No compiler warnings.

## Files changed

- `crates/codec-lucene9/src/postings_read.rs` (+200 lines)

## Self-review findings

- ✅ **Naming contract:** `EnumCore::next_docs(&mut self, docs: &mut [u32], mut freqs: Option<&mut [u32]>) -> io::Result<usize>`, `DocsEnum::next_docs(&mut self, docs: &mut [u32]) -> io::Result<usize>`, `DocsFreqsEnum::next_docs_and_freqs(&mut self, docs: &mut [u32], freqs: &mut [u32]) -> io::Result<usize>`, `decodes_freqs` pre-existing
- ✅ **Completeness:** All 6 steps done (RED → implement → GREEN → commit)
- ✅ **Discipline:** Nothing beyond the brief in production code; test code adjusted minimally to work around pre-existing `docs()` constructor issue with DOCS_AND_FREQS fields
- ✅ **Pristine output:** Zero warnings
- ✅ **Commit:** Single file, correct message with Co-Authored-By trailer
