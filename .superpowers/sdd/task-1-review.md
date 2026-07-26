# Task 1 Review: 骨架——DocBlockBuf + trait 默认 fill + 开关 + driver + CountCollector

**Reviewer:** Task 1 Reviewer
**Commit:** e40d1d0
**Branch:** dev
**Date:** 2026-07-26

---

## Verdict 1: Spec Compliance

### Summary: ✅ COMPLIANT

All 9 steps executed in order. All named items present. All 4 tests present. Nothing added beyond the brief.

### Step-by-step verification

| Step | Requirement | Status |
|------|-------------|--------|
| 1 | Write failing tests (4 tests in `block_tests.rs`) | ✅ |
| 2 | Run tests confirm failure (RED evidence) | ✅ |
| 3 | `DOC_BLOCK` / `DocBlockBuf` / trait default fill in `doc_iter.rs` | ✅ |
| 4 | `SegmentDocIter::next_block` 14-arm dispatch | ✅ |
| 5 | `block_enabled()` in `segment_reader.rs` | ✅ |
| 6 | `Collector::collect_block` + `CountCollector` override in `collector.rs` | ✅ |
| 7 | `drive_blocks` + `search`/`count` branching in `searcher.rs` | ✅ |
| 8 | Run tests confirm pass (GREEN evidence, 277 total) | ✅ |
| 9 | Commit with Chinese subject + Co-Authored-By trailer | ✅ |

### Naming contract verification

| Name | Required | Present | Location |
|------|----------|---------|----------|
| `DOC_BLOCK: usize = 128` | ✅ | ✅ | `doc_iter.rs:18` |
| `DocBlockBuf { docs: [u32;128], freqs: [u32;128], len: usize }` | ✅ | ✅ | `doc_iter.rs:24-28` |
| `DocBlockBuf::new()` | ✅ | ✅ | `doc_iter.rs:31-37` |
| `DocIter::next_block(&mut self, out: &mut DocBlockBuf) -> io::Result<usize>` (default fill) | ✅ | ✅ | `doc_iter.rs:69-84` |
| `Collector::collect_block(&mut self, docs: &[u32], freqs: Option<&[u32]>)` (default per-doc) | ✅ | ✅ | `collector.rs:16-29` |
| `block_enabled() -> bool` (RL_BLOCK, default true) | ✅ | ✅ | `segment_reader.rs:137-140` |
| `drive_blocks<C: Collector>(...) -> io::Result<()>` | ✅ | ✅ | `searcher.rs:19-39` |

### Constraint 1: per-doc path byte-identical

**`search()` (`searcher.rs:63-86`):** The `if block_enabled() { drive_blocks(...); continue; }` guard is inserted BEFORE the existing per-doc loop. When `block_enabled()` returns false (RL_BLOCK=0), execution falls through to the UNCHANGED per-doc loop body (lines 73-83). No existing per-doc line was modified. ✅

**`count()` (`searcher.rs:90-121`):** Same pattern: `if block_enabled() { ...; continue; }` guard before unchanged per-doc loop. Per-doc loop body (lines 109-117) is byte-identical to pre-task code. ✅

**`top_docs()` (`searcher.rs:127-163`):** Not modified per brief ("top_docs 本任务不动"). ✅

### All 4 tests present and named correctly

1. `default_fill_tail_semantics` — edge sizes 0/1/127/128/129/255/256/1000 ✅
2. `drive_blocks_count_parity_single_segment` — LCG-generated 5000-doc set ✅
3. `drive_blocks_doc_base_offset` — doc_base=1000 with 300 docs ✅
4. `collect_block_default_matches_per_doc` — None and Some freq paths ✅

### No extra items beyond brief

- No extra methods, traits, structs, tests, or re-exports added.
- Two `#[allow(...)]` attributes added (`unused_imports` for `DOC_BLOCK`, `dead_code` for `stream_per_doc`). Both justified: the brief's code includes these items reserved for future tasks, and the attributes suppress warnings without removing the items. Acceptable.

### Missing items: NONE

### Extra items: NONE (beyond the two justified `#[allow]` attributes)

---

## Verdict 2: Code Quality

### Summary: **Important:1, Minor:2**

---

### Important:1

**I-1. `SegmentDocIter::next_block` Freqs arm violates trait contract — doesn't fill `out.freqs`**

`doc_iter.rs:1644-1656` (SegmentDocIter::Freqs arm):

```rust
Self::Freqs(f) => {
    let mut n = 0;
    while n < DOC_BLOCK {
        let doc = f.next_doc()?;
        if doc == NO_MORE_DOCS { break; }
        out.docs[n] = doc as u32;
        // MISSING: out.freqs[n] = f.freq();
        n += 1;
    }
    out.len = n;
    Ok(n)
}
```

The trait contract at `doc_iter.rs:68` states:
> "(4) 携带 freq 的迭代器同步填 out.freqs[..n]"

The `Freqs` arm (wrapping `DocsFreqsEnum`) is the ONE variant whose raison d'être is carrying freq data. It fills `out.docs` but NOT `out.freqs`. Since `DocBlockBuf::new()` zeros the arrays and `drive_blocks` reuses the same buf across iterations, `out.freqs` contains either 0s (first block) or stale values from a prior block.

**Impact:** When `needs_freq=true` (only `FreqSumCollector`) and the iterator is `SegmentDocIter::Freqs` (produced by `Query::Term` with `has_freqs=true` at `query.rs:204-207`), `drive_blocks` passes `Some(&out.freqs[..n])` containing 0/stale values. `FreqSumCollector::collect_block` (which inherits the default since no override exists for it) would sum incorrect values.

**Why not Critical:**
- No test exercises this path (block tests use `MaterializedDocIter`, not `Freqs`).
- `FreqSumCollector` has a direct `TermEntry.total_term_freq` fast path for single-term queries; only multi-term queries route through `search()`.
- Task 3 explicitly replaces both Docs/Freqs inline arms with codec-level batch readers — the fix is `out.freqs[n] = f.freq();` in the interim, which is trivial.

**Recommended fix for Task 3:** Add `out.freqs[n] = f.freq();` inside the Freqs arm's while-loop as an interim measure, or confirm Task 3's codec batch reader fills both arrays.

---

### Minor:2

**M-1. `DocBlockBuf` doesn't derive `Default`**

`doc_iter.rs:24-38`: `DocBlockBuf` has `new()` but no `Default` impl. Since `new()` returns a zero-initialized struct, `Default` could be derived. This is purely stylistic — `new()` is the required API per the naming contract.

**M-2. `#[allow(unused_imports)]` and `#[allow(dead_code)]` for reserved items**

`block_tests.rs:30` (`DOC_BLOCK` import) and `block_tests.rs:60` (`stream_per_doc` function): These items are reserved for Tasks 2+ and 8 respectively. The `#[allow]` attributes are a pragmatic workaround to keep the brief's code intact without generating warnings. Acceptable for Task 1, but should be cleaned up when the items are actually used.

---

### Correctness review (no issues found)

| Component | Assessment |
|-----------|------------|
| **Default fill loop** (`doc_iter.rs:69-84`) | Correct. Two-phase `matches()` absorbed. `NO_MORE_DOCS` terminates. `out.len = n` set. `out.docs[n] = d as u32` only after matches=true. |
| **14-arm dispatch** (`doc_iter.rs:1627-1669`) | All 14 `SegmentDocIter` variants handled. Docs/Freqs inline (codec types, no `DocIter` impl). Other 12 delegate to `next_block(out)`. Match arms exhaustive. |
| **`block_enabled()`** (`segment_reader.rs:137-140`) | Mirrors `bitmap_enabled()` pattern exactly. `OnceLock<bool>` is correct for process-wide env-var latch. Default `true` when `RL_BLOCK` unset; `false` only when `RL_BLOCK=0`. |
| **`collect_block` default** (`collector.rs:16-29`) | `Some(f)` path: `enumerate + f[i]` — correct. `None` path: freq=1 — correct. Both call `self.collect(d as i32, ...)`. |
| **`CountCollector::collect_block` override** (`collector.rs:42-44`) | `self.count += docs.len() as u64` — ignores `_freqs` as expected (count doesn't need freq). Consistent with per-doc `collect` which does `self.count += 1`. |
| **`drive_blocks`** (`searcher.rs:19-39`) | Correct loop. `doc_base` addition guarded by `!= 0` (minor optimization, correct). `needs_freq.then(\|\| &out.freqs[..n])` properly threads `Option`. No `matches()` call (absorbed by `next_block`). |
| **`search()` branching** (`searcher.rs:63-86`) | Block path: `drive_blocks` with `doc_base as u32` cast (safe — `doc_base` from `leaves()` is non-negative). Per-doc path: unchanged, uses `doc_base + doc` (i32 arithmetic). Both produce same global docIDs. |
| **`count()` branching** (`searcher.rs:90-121`) | Block path sums `n as u64`. Per-doc path sums `1` per match. Both skip `doc_base` (correct — count doesn't need global IDs). |

### Test quality (non-vacuous)

| Test | Vacuous? | Reason |
|------|----------|--------|
| `default_fill_tail_semantics` | No | Tests 8 edge sizes including 0, boundary (127/128/129), multi-block (255/256), large (1000). Asserts exact Vec equality. |
| `drive_blocks_count_parity_single_segment` | No | 5000-doc LCG set with 100K universe (5% density). Two independent paths compared. Both `count.count == docs.len()` AND `count.count == per_doc` asserted. |
| `drive_blocks_doc_base_offset` | No | 300 docs × 3 stride + doc_base=1000. Verifies exact docID values AND freq=1 for all. |
| `collect_block_default_matches_per_doc` | No | Tests both `None` (freq=1) and `Some(&[2,3,4,5])` paths. Asserts exact doc and freq vectors. |

### Commit discipline

- Single commit `e40d1d0` ✅
- Chinese subject line ✅
- `Co-Authored-By: Claude <noreply@anthropic.com>` trailer ✅
- Subject format: `feat(batch): 块迭代骨架——...` ✅

---

## Items that cannot be verified from diff

1. **277 tests passed:** Report claims green suite. I verified `cargo check` produces no warnings. I did NOT re-run the test suite per instructions. Report evidence (test output snippets) is consistent.

2. **P1-1 / P1-3 zero regression:** This is Task 8's responsibility (1M regression battery). Cannot verify from Task 1 diff alone. The per-doc path is byte-identical when `RL_BLOCK=0`, so no regression is expected from code changes.

3. **Four-way reconciliation (CLI + JNI):** Constraint 4 requires "默认 ON 影响所有消费方 → 四路对账". Task 1 adds the block path with `block_enabled()` default true. The actual four-way reconciliation (verifying CLI and JNI produce identical results with block ON vs OFF) is Task 8's scope.

---

## Final Verdicts

**Verdict 1 — Spec compliance:** ✅ COMPLIANT. All 9 steps, all named items, all 4 tests, no missing items, no extra items (beyond two justified `#[allow]` attributes).

**Verdict 2 — Code quality:** Approved with **Important:1, Minor:2**.
- I-1: Freqs arm doesn't fill `out.freqs` (trait contract violation, latent bug, Task 3 must fix).
- M-1: `DocBlockBuf` doesn't derive `Default` (stylistic).
- M-2: `#[allow]` for reserved items (acceptable, clean up later).

**Overall:** APPROVED for merge with the understanding that Task 3 MUST fix the Freqs arm `out.freqs` fill before any frequency-sensitive collector is exercised through the block path.
