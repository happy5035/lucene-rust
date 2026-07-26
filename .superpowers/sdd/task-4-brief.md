### Task 4: 共享块代数——intersect / andnot / kway-union

**Files:**
- Modify: `crates/core/src/search/doc_iter.rs`（`SegmentDocIter` 定义 :1495 之前新增代数区）
- Modify: `crates/core/src/search/block_tests.rs`（代数单测）

**Interfaces:**
- Consumes: 无（纯函数）
- Produces: `block_intersect(a, b, out) -> (usize, usize, usize)`、`block_andnot(a, b, out) -> (usize, usize, usize)`、`kway_union(heads, consumed, out) -> usize`——Task 5/6 组合器覆写的内核；Phase 2 SIMD 只换这三个函数内核，签名不动

- [ ] **Step 1: 写失败测试——代数四象限**

`block_tests.rs` 尾部追加：

```rust
use super::doc_iter::{block_andnot, block_intersect, kway_union};

fn run(f: impl Fn(&[u32], &[u32], &mut [u32]) -> (usize, usize, usize), a: &[u32], b: &[u32]) -> Vec<u32> {
    // 分片消费至耗尽（模拟组合器跨调用状态机）
    let mut out = vec![0u32; 128];
    let mut res = Vec::new();
    let (mut pa, mut pb) = (0, 0);
    loop {
        let (ca, cb, n) = f(&a[pa..], &b[pb..], &mut out);
        res.extend_from_slice(&out[..n]);
        pa += ca;
        pb += cb;
        if n == 0 || (pa == a.len() && (ca == 0 || cb == 0 && pb == b.len())) {
            break;
        }
        if ca == 0 && cb == 0 {
            break;
        }
    }
    res
}

fn expect_intersect(a: &[u32], b: &[u32]) -> Vec<u32> {
    a.iter().filter(|x| b.contains(x)).copied().collect()
}

fn expect_andnot(a: &[u32], b: &[u32]) -> Vec<u32> {
    a.iter().filter(|x| !b.contains(x)).copied().collect()
}

#[test]
fn algebra_quadrants() {
    let cases: Vec<(Vec<u32>, Vec<u32>)> = vec![
        (vec![], vec![]),
        (vec![1, 2, 3], vec![]),
        (vec![], vec![1, 2, 3]),
        (vec![1, 3, 5], vec![2, 4, 6]),          // 不相交
        (vec![1, 2, 3], vec![1, 2, 3]),          // 全等
        (vec![2, 4], vec![1, 2, 3, 4, 5]),       // 包含
        ((0..300).step_by(2).collect(), (0..300).step_by(3).collect()), // 交错跨块
        ((0..128).collect(), (0..256).collect()),                      // 128 整数倍
        ((0..127).collect(), (0..129).collect()),                      // 尾块
    ];
    for (a, b) in cases {
        assert_eq!(run(block_intersect, &a, &b), expect_intersect(&a, &b), "intersect {a:?} {b:?}");
        assert_eq!(run(block_andnot, &a, &b), expect_andnot(&a, &b), "andnot {a:?} {b:?}");
        // andnot 反对称
        assert_eq!(run(block_andnot, &b, &a), expect_andnot(&b, &a), "andnot rev {a:?} {b:?}");
    }
}

#[test]
fn kway_union_dedup_and_order() {
    let mut lcg = Lcg(99);
    for _ in 0..30 {
        let k = 2 + (lcg.next_u32() % 5) as usize;
        let sets: Vec<Vec<u32>> = (0..k)
            .map(|_| lcg.doc_set(3_000, (lcg.next_u32() % 400) as usize))
            .collect();
        // 期望 = 并集去重升序
        let mut expect: Vec<u32> = sets.iter().flatten().copied().collect();
        expect.sort_unstable();
        expect.dedup();
        // 分片消费
        let mut got = Vec::new();
        let mut pos = vec![0usize; k];
        let mut out = vec![0u32; 64]; // 故意 <128 触发多次归并
        loop {
            let heads: Vec<&[u32]> = sets.iter().enumerate().map(|(i, s)| &s[pos[i]..]).collect();
            let mut consumed = vec![0usize; k];
            let n = kway_union(&heads, &mut consumed, &mut out);
            got.extend_from_slice(&out[..n]);
            for i in 0..k {
                pos[i] += consumed[i];
            }
            if n == 0 {
                break;
            }
        }
        assert_eq!(got, expect, "k={k}");
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p rustlucene-core algebra_ 2>&1 | tail -5; cargo test -p rustlucene-core kway_ 2>&1 | tail -5`
Expected: 编译错误——三个函数未定义

- [ ] **Step 3: 实现三个代数内核**

在 `doc_iter.rs` 的 `// ── SegmentDocIter ──` 注释行（:1493）之前插入：

```rust
// ── 块代数内核（spec 2026-07-26 §4；Phase 2 SIMD 只换这里） ─────────

/// 双指针 intersect，部分消费语义：对 a/b 前缀求交写入 out（至多
/// out.len() 个），返回 (消费 a 数, 消费 b 数, 产出数)。产出满或
/// 某侧耗尽即停——调用方按返回值推进游标跨调用续算。
/// 输入要求：a/b 升序（块契约）。
fn block_intersect(a: &[u32], b: &[u32], out: &mut [u32]) -> (usize, usize, usize) {
    let (mut ia, mut ib, mut n) = (0, 0, 0);
    while ia < a.len() && ib < b.len() && n < out.len() {
        let (x, y) = (a[ia], b[ib]);
        if x == y {
            out[n] = x;
            n += 1;
            ia += 1;
            ib += 1;
        } else if x < y {
            ia += 1;
        } else {
            ib += 1;
        }
    }
    (ia, ib, n)
}

/// slice 差集 a \ b，部分消费语义同 block_intersect。注意：b 侧消费
/// 只推进到"已确认 < a 当前尾"的前缀——b 游标跨调用留存（Excl 的
/// prohibited 块语义）。
fn block_andnot(a: &[u32], b: &[u32], out: &mut [u32]) -> (usize, usize, usize) {
    let (mut ia, mut ib, mut n) = (0, 0, 0);
    while ia < a.len() && n < out.len() {
        let x = a[ia];
        while ib < b.len() && b[ib] < x {
            ib += 1;
        }
        if ib < b.len() && b[ib] == x {
            ib += 1; // 排除；b 该元素已消费
        } else {
            out[n] = x;
            n += 1;
        }
        ia += 1;
    }
    (ia, ib, n)
}

/// k 路有序 slice 归并去重，out 满即停。consumed[i] 写回各 head 消费
/// 数（调用方初始化长度 = heads.len()）。k 小（bool 子句数）→ 线性扫
/// 最小头，不上堆（堆化是 bool-bench-report §11 P2 议题）。
fn kway_union(heads: &[&[u32]], consumed: &mut [usize], out: &mut [u32]) -> usize {
    debug_assert_eq!(heads.len(), consumed.len());
    let mut n = 0;
    let mut last: Option<u32> = None;
    while n < out.len() {
        // 选最小头
        let mut best: Option<(usize, u32)> = None;
        for (i, h) in heads.iter().enumerate() {
            let rest = &h[consumed[i]..];
            if let Some(&d) = rest.first() {
                if best.is_none_or(|(_, bd)| d < bd) {
                    best = Some((i, d));
                }
            }
        }
        let Some((_, d)) = best else { break };
        // 推进所有等于 d 的头（去重）
        for (i, h) in heads.iter().enumerate() {
            let rest = &h[consumed[i]..];
            if rest.first() == Some(&d) {
                consumed[i] += 1;
            }
        }
        if last != Some(d) {
            out[n] = d;
            n += 1;
            last = Some(d);
        }
    }
    n
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p rustlucene-core algebra_ kway_ 2>&1 | tail -4`
Expected: 2 测试 PASS

Run: `cargo test --workspace 2>&1 | tail -3`
Expected: 全绿（281 passed）

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/search/doc_iter.rs crates/core/src/search/block_tests.rs
git commit -m "$(cat <<'EOF'
feat(batch): 共享块代数内核 block_intersect / block_andnot / kway_union

spec 2026-07-26 §4：部分消费语义（组合器跨调用游标状态机基础），
标量双指针/线性归并——Phase 2 SIMD 只换内核不改签名。四象限 +
随机 30 组 k 路（k=2..6，out=64 故意触发多轮）对拍。

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```

---

