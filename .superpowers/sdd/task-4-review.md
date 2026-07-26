# Task 4 Review: 共享块代数——intersect / andnot / kway-union

## Verdict 1: Spec Compliance ✅

All five steps completed correctly:
- Step 1: Tests written with borrow-checker adjustment (mechanical, documented)
- Step 2: RED confirmed (compile errors for missing functions)
- Step 3: Three kernels implemented verbatim from brief (modulo `#[allow(dead_code)]` for warnings)
- Step 4: GREEN confirmed (283 passed, 0 failed)
- Step 5: Single commit with Chinese subject, Co-Authored-By trailer

**Naming contract:** ✅ All three functions match exact signatures specified.

**Scope discipline:** ✅ No SIMD, no extra variants, scalar-only implementations as required.

**Drift from brief:** ✅ Only the documented borrow-checker fix and `#[allow(dead_code)]` addition—both mechanical, both explained in report.

## Verdict 2: Code Quality

### Hand-Trace Analysis

#### block_intersect — ✅ CORRECT

**Partial-fill scenario:** `a=[1,2,3,4,5], b=[2,4,6], out.len()=1`

**Call 1:** Loop until n=1 (out full). Match at a[1]=b[0]=2, emit, exit.
- Returns (2, 1, 1). Caller advances pa=2, pb=1.

**Call 2:** `block_intersect(a[2..], b[1..], out[1])` where a[2..]=[3,4,5], b[1..]=[4,6].
- Skip a[2..][0]=3 (no match in b), match at a[2..][1]=b[1..][0]=4, emit, exit.
- Returns (2, 1, 1). Caller advances pa=4, pb=2.

**Call 3:** `block_intersect(a[4..], b[2..], out[1])` where a[4..]=[5], b[2..]=[6].
- Skip a[4..][0]=5, a exhausted, exit.
- Returns (1, 0, 0). Caller advances pa=5, pb=2.

**Result:** [2, 4]. ✅ Consumed values allow correct resumption without loss/duplication.

**Boundary cases verified:**
- Empty inputs: Loop exits immediately, returns (0, 0, 0). ✅
- Single element: Works correctly. ✅
- Disjoint: All elements skipped, no output. ✅
- Identical: All elements match, fills out buffer correctly. ✅
- out.len()==0: Loop condition `n < out.len()` is false, returns (0, 0, 0). ✅

#### block_andnot — ✅ CORRECT

**Partial-fill scenario:** `a=[1,3,5,7,9], b=[2,4,6,8], out.len()=2`

**Call 1:** Loop until n=2 (out full).
- a[0]=1: b[0]=2 > 1, so emit 1, n=1, ia=1, ib=0
- a[1]=3: advance ib past b[0]=2, b[1]=4 > 3, emit 3, n=2, ia=2, ib=1
- Exit (n == out.len()).
- Returns (2, 1, 2). Caller advances pa=2, pb=1.

**Call 2:** `block_andnot(a[2..], b[1..], out[2])` where a[2..]=[5,7,9], b[1..]=[4,6,8].
- a[2..][0]=5: advance ib past b[1..][0]=4, b[1..][1]=6 > 5, emit 5, n=1, ia=1, ib=1
- a[2..][1]=7: advance ib past b[1..][1]=6, b[1..][2]=8 > 7, emit 7, n=2, ia=2, ib=2
- Exit.
- Returns (2, 2, 2). Caller advances pa=4, pb=3.

**Call 3:** `block_andnot(a[4..], b[3..], out[2])` where a[4..]=[9], b[3..]=[8].
- a[4..][0]=9: advance ib past b[3..][0]=8, b exhausted, emit 9, n=1, ia=1, ib=1
- Exit (ia == a.len()).
- Returns (1, 1, 1). Caller advances pa=5, pb=4.

**Result:** [1, 3, 5, 7, 9] = a \ b. ✅ Correct partial-fill semantics.

**Key insight:** The inner `while ib < b.len() && b[ib] < x` loop correctly consumes b elements that are definitively less than a's current element, but stops before consuming b elements that might match future a elements. This is the "b cursor persists across calls" semantics mentioned in the doc comment.

**Boundary cases verified:**
- Empty a: Loop exits immediately, returns (0, 0, 0). ✅
- Empty b: All a elements emitted, returns (a.len(), 0, min(a.len(), out.len())). ✅
- a ⊆ b: All a elements excluded, returns (a.len(), a.len(), 0). ✅
- a ∩ b = ∅: All a elements emitted, returns (a.len(), 0, min(a.len(), out.len())). ✅
- out.len()==0: Loop condition `n < out.len()` is false, returns (0, 0, 0). ✅

#### kway_union — ✅ CORRECT

**Partial-fill scenario:** `sets=[[1,3,5], [2,3,6]], out.len()=2`

**Call 1:** Loop until n=2 (out full).
- Find min: d=1 from set[0]. Advance set[0], consumed=[1,0]. Emit 1, n=1, last=Some(1).
- Find min: d=2 from set[1]. Advance set[1], consumed=[1,1]. Emit 2, n=2, last=Some(2).
- Exit.
- Returns 2, consumed=[1,1]. Caller advances pos=[1,1].

**Call 2:** `kway_union(heads=[[3,5],[3,6]], consumed=[0,0], out[2])`.
- Find min: d=3 from set[0]. Advance ALL heads with d=3: both set[0] and set[1] advance, consumed=[1,1]. Emit 3, n=1, last=Some(3).
- Find min: d=5 from set[0]. Advance set[0], consumed=[2,1]. Emit 5, n=2, last=Some(5).
- Exit.
- Returns 2, consumed=[2,1]. Caller advances pos=[3,2].

**Call 3:** `kway_union(heads=[[],[6]], consumed=[0,0], out[2])`.
- Find min: d=6 from set[1]. Advance set[1], consumed=[0,1]. Emit 6, n=1, last=Some(6).
- Find min: None (all heads exhausted). Break.
- Returns 1, consumed=[0,1]. Caller advances pos=[3,3].

**Result:** [1, 2, 3, 5, 6]. ✅ Deduplication works correctly across calls because the "advance all heads equal to d" loop consumes all copies of d in a single iteration.

**Critical verification:** When d=3 appears in both sets, the second loop (lines 1677-1682) advances BOTH heads, so consumed=[1,1] after processing d=3. This ensures no duplicate emission even when the buffer fills and we resume.

**Boundary cases verified:**
- k==0: `heads.is_empty()`, first loop finds no min, breaks immediately, returns 0. ✅
- k==1: Works like a simple copy with dedup. ✅
- All duplicates: "Advance all heads equal to d" consumes all copies, emits once. ✅
- out.len()==0: Loop condition `n < out.len()` is false, returns 0. ✅
- Empty heads: First loop finds no min, breaks, returns 0. ✅

**⚠️ Cannot verify:** The `debug_assert_eq!(heads.len(), consumed.len())` at line 1661 is only checked in debug builds. In release builds, a mismatch would cause index-out-of-bounds panics or silent corruption. However, this is acceptable for a kernel function where the caller is responsible for correct setup.

### Test Coverage Analysis

**algebra_quadrants test:** 
- ✅ Covers 9 diverse cases (empty, disjoint, identical, subset, cross-block, multiples of 128, tail block)
- ✅ Tests both intersect and andnot, including andnot antisymmetry
- ❌ **Does NOT assert on consumed values directly** — only checks final output via `run()` helper
- The `run()` helper uses consumed values to advance cursors, but doesn't assert on them
- **Risk:** A bug in consumed values that still produces correct output would pass this test but corrupt Tasks 5/6

**kway_union_dedup_and_order test:**
- ✅ 30 random rounds with k=2..6, universe=3000, out=64 (forces multi-call)
- ❌ **Does NOT assert on consumed values directly** — only checks final output
- **Risk:** Same as above

**Recommendation for Tasks 5/6:** When implementing the combinator overrides, add unit tests that directly assert on consumed values for specific partial-fill scenarios. For example:

```rust
#[test]
fn block_intersect_consumed_values() {
    let a = [1, 2, 3, 4, 5];
    let b = [2, 4, 6];
    let mut out = [0u32; 1];
    let (ca, cb, n) = block_intersect(&a, &b, &mut out);
    assert_eq!((ca, cb, n), (2, 1, 1));
    assert_eq!(out[0], 2);
}
```

This pins the exact resumption coordinates and prevents the "consumption-coordinate confusion" bug class mentioned in the review instructions.

### Code Quality Issues

#### Critical: 0

#### Important: 1

**I-1: Tests don't pin consumed values (block_tests.rs:30-48, 80-111)**

The quadrant tests only verify final output, not intermediate consumed values. This allows bugs where consumed values are wrong but the output is still correct (e.g., if consumed values are off by one but the `run()` helper's termination condition compensates).

**Impact:** Tasks 5/6 will build combinator overrides directly on these consumed-value semantics. A subtle bug here would propagate silently.

**Recommendation:** Add 2-3 targeted tests that directly assert on consumed values for partial-fill scenarios (see example above). This is low-effort, high-value insurance.

#### Minor: 2

**M-1: Doc comment could clarify kway_union's consumed initialization (doc_iter.rs:1660)**

The doc comment says "consumed[i] 写回各 head 消费数（调用方初始化长度 = heads.len()）" but doesn't explicitly state that consumed should be initialized to zeros. The test code does this correctly (line 99: `let mut consumed = vec![0usize; k]`), but the API contract could be clearer.

**Recommendation:** Add "调用方每次调用前初始化为 0" to the doc comment.

**M-2: kway_union uses linear scan for min-finding (doc_iter.rs:1666-1674)**

The doc comment acknowledges this is intentional ("k 小（bool 子句数）→ 线性扫最小头，不上堆"), which is correct for k≤6 (typical bool query clause count). However, if k grows larger (e.g., a disjunction over 50 clauses), this becomes O(k²) per output element.

**Recommendation:** Add a comment or assertion suggesting k≤10 for optimal performance, or consider heap-based merge for larger k in Phase 2.

### Summary

**Spec compliance:** ✅ All requirements met.

**Code quality:** Approved with Important:1, Minor:2

- All three kernels have correct partial-fill semantics (verified by hand-trace)
- Boundary cases handled correctly
- The only Important issue is that tests don't directly assert on consumed values, leaving a gap that Tasks 5/6 could fall through

**Action items:**
1. **Before Tasks 5/6:** Add 2-3 unit tests that directly assert on consumed values for partial-fill scenarios (I-1)
2. **Optional:** Clarify kway_union's consumed initialization contract (M-1)
3. **Phase 2:** Consider heap-based merge for k>10 (M-2)

**Overall:** The implementation is correct and well-documented. The partial-fill semantics are subtle but handled properly. The only risk is insufficient test coverage of consumed values, which should be addressed before Tasks 5/6 build on these kernels.
