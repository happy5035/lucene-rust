### Task 5: 组合器覆写（一）——Excluding / ConjOver / DisjOver

**Files:**
- Modify: `crates/core/src/search/doc_iter.rs`（BlockCursor 辅助 + ExcludingDocIter :1424-1491 / ConjOverDocIter :1021-1122 / DisjOverDocIter :1301-1419 的 impl 追加 next_block）
- Modify: `crates/core/src/search/block_tests.rs`（组合器对拍）

**Interfaces:**
- Consumes: Task 4 三代数函数；子迭代器 `SegmentDocIter::next_block`（叶子已真块，phrase 默认 fill——输出都已 matches 确认）
- Produces: 三个 Over/Excl 组合器的 `next_block` 覆写 + `BlockCursor` 复用件（Task 6 PostingsIter 组合器复用同款游标模式）

- [ ] **Step 1: 写失败测试——组合器对拍**

`block_tests.rs` 尾部追加：

```rust
use super::doc_iter::{ConjOverDocIter, DisjOverDocIter, ExcludingDocIter};

fn conj_over(children: Vec<Vec<u32>>) -> SegmentDocIter {
    let subs: Vec<SegmentDocIter> = children.iter().map(|c| leaf(c)).collect();
    SegmentDocIter::ConjOver(ConjOverDocIter::new(subs).unwrap())
}

fn disj_over(children: Vec<Vec<u32>>) -> SegmentDocIter {
    let subs: Vec<SegmentDocIter> = children.iter().map(|c| leaf(c)).collect();
    SegmentDocIter::DisjOver(DisjOverDocIter::new(subs).unwrap())
}

fn excl(main: Vec<u32>, prohibited: Vec<u32>) -> SegmentDocIter {
    SegmentDocIter::Excluding(ExcludingDocIter::new(leaf(&main), leaf(&prohibited)))
}

fn expect_conj(sets: &[Vec<u32>]) -> Vec<u32> {
    let mut r: Vec<u32> = sets[0].clone();
    for s in &sets[1..] {
        r.retain(|x| s.contains(x));
    }
    r
}

fn expect_disj(sets: &[Vec<u32>]) -> Vec<u32> {
    let mut r: Vec<u32> = sets.iter().flatten().copied().collect();
    r.sort_unstable();
    r.dedup();
    r
}

#[test]
fn combinator_block_vs_per_doc_random() {
    let mut lcg = Lcg(1234);
    for round in 0..40 {
        let k = 2 + (lcg.next_u32() % 3) as usize;
        let sets: Vec<Vec<u32>> = (0..k)
            .map(|_| lcg.doc_set(4_000, (lcg.next_u32() % 500) as usize))
            .collect();
        // ConjOver
        let mut a = conj_over(sets.clone());
        let mut b = conj_over(sets.clone());
        assert_eq!(stream_block(&mut a), stream_per_doc(&mut b), "conj round={round}");
        assert_eq!(stream_block(&mut conj_over(sets.clone())), expect_conj(&sets));
        // DisjOver
        let mut a = disj_over(sets.clone());
        let mut b = disj_over(sets.clone());
        assert_eq!(stream_block(&mut a), stream_per_doc(&mut b), "disj round={round}");
        assert_eq!(stream_block(&mut disj_over(sets.clone())), expect_disj(&sets));
        // Excluding
        let (m, p) = (sets[0].clone(), sets[1].clone());
        let expect: Vec<u32> = m.iter().filter(|x| !p.contains(x)).copied().collect();
        let mut a = excl(m.clone(), p.clone());
        let mut b = excl(m, p);
        assert_eq!(stream_block(&mut a), stream_per_doc(&mut b), "excl round={round}");
        assert_eq!(stream_block(&mut a), expect);
    }
}

#[test]
fn combinator_edge_shapes() {
    // 空交 / 空集子句 / 128 整数倍 / 全等 / 尾块
    let e: Vec<u32> = vec![];
    assert_eq!(stream_block(&mut conj_over(vec![vec![1, 2], e.clone()])), vec![]);
    assert_eq!(stream_block(&mut disj_over(vec![e.clone(), vec![3]])), vec![3]);
    let full: Vec<u32> = (0..256).collect();
    assert_eq!(stream_block(&mut excl(full.clone(), e)), full);
    assert_eq!(stream_block(&mut excl(e.clone(), full.clone())), vec![]);
    let a: Vec<u32> = (0..300).step_by(2).collect();
    let b: Vec<u32> = (0..300).step_by(3).collect();
    let expect: Vec<u32> = a.iter().filter(|x| !b.contains(x)).copied().collect();
    assert_eq!(stream_block(&mut excl(a, b)), expect);
}
```

注：`leaf(&[])` —— `MaterializedBitmap::of(&[])` 空集合法（materialized.rs:117-123 已有测试）；`ConjOverDocIter::new` 对空集子句返回 doc=NO_MORE_DOCS（:1037-1041）→ 空交语义 ✓。`ExcludingDocIter` 的 main 为空：首次 next_doc 即 NO_MORE ✓。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p rustlucene-core combinator_ 2>&1 | tail -5`
Expected: FAIL 或编译通过但断言挂——组合器尚无 next_block 覆写，走默认 fill……**默认 fill 语义正确**（循环 next_doc+matches），对拍可能 PASS。与 Task 3 同理：本测试是覆写后的回归守卫；先确认当前 PASS（锁定语义基线），覆写后必须继续 PASS。

- [ ] **Step 3: BlockCursor 辅助件**

在 Task 4 代数区（`block_andnot` 之后、`kway_union` 之前或之后均可）追加：

```rust
/// 组合器块路径的 child 游标（spec §4）：持有 child 的当前块切片窗口。
/// 18.7KB 的 SegmentDocIter 子迭代器不进此结构——只存 1KB 块缓冲（堆）。
struct BlockCursor {
    buf: Box<DocBlockBuf>,
    pos: usize,
    len: usize,
    exhausted: bool,
}

impl BlockCursor {
    fn new() -> BlockCursor {
        BlockCursor {
            buf: Box::new(DocBlockBuf::new()),
            pos: 0,
            len: 0,
            exhausted: false,
        }
    }

    /// 拉 child 下一块。返回 false = child 耗尽（此后 remaining() 恒空）。
    fn refill<I: DocIter>(&mut self, it: &mut I) -> io::Result<bool> {
        if self.exhausted {
            return Ok(false);
        }
        let n = it.next_block(&mut self.buf)?;
        self.pos = 0;
        self.len = n;
        if n == 0 {
            self.exhausted = true;
            return Ok(false);
        }
        Ok(true)
    }

    /// 当前块未消费切片。
    fn remaining(&self) -> &[u32] {
        &self.buf.docs[self.pos..self.len]
    }

    /// 当前块尾 doc（空切片 = None）。
    fn max_remaining(&self) -> Option<u32> {
        (self.pos < self.len).then(|| self.buf.docs[self.len - 1])
    }

    /// 推进游标 n 个（代数内核返回值）。
    fn consume(&mut self, n: usize) {
        self.pos += n;
    }
}

/// 窗口定位（n 元合取逐元素路径用）：消费块内 < e 的前缀，跨块
/// refill 直到首元素 >= e。返回 `Some(首元素 == e)`（contains 判定）/
/// `None` = child 耗尽。前向单调，对同一 child 以递增 e 序列调用。
fn position_seg(
    curs: &mut BlockCursor,
    child: &mut SegmentDocIter,
    e: u32,
) -> io::Result<Option<bool>> {
    loop {
        while !curs.remaining().is_empty() && curs.remaining()[0] < e {
            curs.consume(1);
        }
        if let Some(head) = curs.remaining().first() {
            return Ok(Some(*head == e));
        }
        if !curs.refill(child)? {
            return Ok(None);
        }
    }
}
```

- [ ] **Step 4: ExcludingDocIter::next_block**

给 `ExcludingDocIter` 结构体（:1424-1428）加游标字段：

```rust
pub struct ExcludingDocIter {
    main: Box<SegmentDocIter>,
    prohibited: Box<SegmentDocIter>,
    doc: i32,
    // 块路径游标（per-doc 路径不读；18.7KB 子迭代器仍在 Box 里）
    mcur: BlockCursor,
    pcur: BlockCursor,
}
```

`ExcludingDocIter::new`（:1431-1437）补字段：

```rust
    pub fn new(main: SegmentDocIter, prohibited: SegmentDocIter) -> ExcludingDocIter {
        ExcludingDocIter {
            main: Box::new(main),
            prohibited: Box::new(prohibited),
            doc: -1,
            mcur: BlockCursor::new(),
            pcur: BlockCursor::new(),
        }
    }
```

`impl DocIter for ExcludingDocIter`（:1467-1491）尾部追加覆写：

```rust
    fn next_block(&mut self, out: &mut DocBlockBuf) -> io::Result<usize> {
        let mut prod = 0;
        while prod < DOC_BLOCK {
            // must 块就位
            if self.mcur.remaining().is_empty() && !self.mcur.refill(&mut self.main)? {
                break; // must 耗尽
            }
            // prohibited 块推进到与 must 当前块重叠（块级跳过——替代
            // per-doc 逐候选 advance 病理，spec §1.1 / P1-1 同源）
            let m_lo = self.mcur.remaining()[0];
            loop {
                match self.pcur.max_remaining() {
                    Some(p_hi) if p_hi >= m_lo => break,
                    _ => {
                        if !self.pcur.refill(&mut self.prohibited)? {
                            // prohibited 耗尽：must 剩余全部命中
                            let rest = self.mcur.remaining();
                            let take = rest.len().min(DOC_BLOCK - prod);
                            out.docs[prod..prod + take].copy_from_slice(&rest[..take]);
                            self.mcur.consume(take);
                            prod += take;
                            self.doc = out.docs[prod - 1] as i32;
                            continue;
                        }
                    }
                }
            }
            let (cm, cp, n) = block_andnot(
                self.mcur.remaining(),
                self.pcur.remaining(),
                &mut out.docs[prod..],
            );
            self.mcur.consume(cm);
            self.pcur.consume(cp);
            prod += n;
        }
        if prod > 0 {
            self.doc = out.docs[prod - 1] as i32;
        } else if self.mcur.exhausted {
            self.doc = NO_MORE_DOCS;
        }
        out.len = prod;
        Ok(prod)
    }
```

- [ ] **Step 5: ConjOverDocIter::next_block**

`ConjOverDocIter` 结构体（:1021-1025）加字段 `curs: Vec<BlockCursor>`，`new`（:1030-1044）初始化 `curs: sub.iter().map(|_| BlockCursor::new()).collect()`。impl 尾部追加：

```rust
    fn next_block(&mut self, out: &mut DocBlockBuf) -> io::Result<usize> {
        let mut prod = 0;
        'blk: while prod < DOC_BLOCK {
            // child0 块就位（耗尽 → 整体结束）
            if self.curs[0].remaining().is_empty() && !self.curs[0].refill(&mut self.sub[0])? {
                break 'blk;
            }
            if self.sub.len() == 2 {
                // 二元快路径（bench 主导形状）：slice intersect，ca/cb
                // 直接是两侧块消费数，部分消费语义天然正确。
                while self.curs[1].max_remaining().is_none_or(|hi| hi < self.curs[0].remaining()[0]) {
                    if !self.curs[1].refill(&mut self.sub[1])? {
                        break 'blk; // child1 耗尽 → 交耗尽
                    }
                }
                let (ca, cb, n) = block_intersect(
                    self.curs[0].remaining(),
                    self.curs[1].remaining(),
                    &mut out.docs[prod..],
                );
                self.curs[0].consume(ca);
                self.curs[1].consume(cb);
                prod += n;
                continue; // n=0 时 ca/cb 已推进，安全续环（两侧非空时必有推进）
            }
            // n 元通用路径：逐元素定位其余 child 窗口（正确性优先于
            // 二元 slice 快路径——scratch 折叠的消费坐标映射易错）。
            let c0_len = self.curs[0].remaining().len();
            let mut used = 0;
            for idx in 0..c0_len {
                let e = self.curs[0].buf.docs[self.curs[0].pos + idx];
                used += 1;
                let mut hit = true;
                for i in 1..self.sub.len() {
                    let (curs, sub) = (&mut self.curs, &mut self.sub);
                    match position_seg(&mut curs[i], &mut sub[i], e)? {
                        None => {
                            // child 耗尽 → 交耗尽
                            self.curs[0].consume(used);
                            self.doc = NO_MORE_DOCS;
                            out.len = prod;
                            return Ok(prod);
                        }
                        Some(false) => {
                            hit = false;
                            break; // 后续 child 无需定位（前向单调，下轮 e' > e 续推）
                        }
                        Some(true) => {}
                    }
                }
                if hit {
                    // 命中：各 child 首元素 == e（position_seg 保证），消费
                    for i in 1..self.sub.len() {
                        self.curs[i].consume(1);
                    }
                    out.docs[prod] = e;
                    prod += 1;
                    if prod == DOC_BLOCK {
                        break;
                    }
                }
            }
            self.curs[0].consume(used);
        }
        if prod > 0 {
            self.doc = out.docs[prod - 1] as i32;
        } else if self.curs.iter().all(|c| c.exhausted || c.remaining().is_empty()) {
            self.doc = NO_MORE_DOCS;
        }
        out.len = prod;
        Ok(prod)
    }
```

正确性要点：`position_seg` 把 child 窗口推进到首元素 ≥ e（块内 consume + 跨块 refill），`Some(head == e)` 即 contains；hit=false 时不消费任何 child（e 不属交集），后续 child 的定位缺口由下一个更大的 e 前向补齐；child0 的消费按 `used` 整块结算。二元形状（and 两词项，bench 主力）走 `block_intersect` 快路径。

- [ ] **Step 6: DisjOverDocIter::next_block**

`DisjOverDocIter` 结构体（:1301-1306）加 `curs: Vec<BlockCursor>`，`new` 初始化。impl 尾部追加：

```rust
    fn next_block(&mut self, out: &mut DocBlockBuf) -> io::Result<usize> {
        // k 路块归并：heads = 各 child 未消费切片，kway_union 满 128 即停。
        // matches() 已由 child 的 next_block 吸收（子句恒单阶段亦无害）。
        let mut prod = 0;
        while prod < DOC_BLOCK {
            // 耗尽 child 的游标补块
            let mut any = false;
            for i in 0..self.sub.len() {
                if self.curs[i].remaining().is_empty() && !self.curs[i].exhausted {
                    self.curs[i].refill(&mut self.sub[i])?;
                }
                if !self.curs[i].remaining().is_empty() {
                    any = true;
                }
            }
            if !any {
                break;
            }
            let heads: Vec<&[u32]> = (0..self.sub.len())
                .map(|i| self.curs[i].remaining())
                .collect();
            let mut consumed = vec![0usize; self.sub.len()];
            let n = kway_union(&heads, &mut consumed, &mut out.docs[prod..]);
            for i in 0..self.sub.len() {
                self.curs[i].consume(consumed[i]);
            }
            prod += n;
            if n == 0 {
                break;
            }
        }
        if prod > 0 {
            self.doc = out.docs[prod - 1] as i32;
        } else {
            self.doc = NO_MORE_DOCS;
        }
        out.len = prod;
        Ok(prod)
    }
```

注：`heads` 借用 `self.curs` 的同时 `consume` 需要 `&mut self.curs`——借用冲突。解法：`kway_union` 调用结束（heads 生命周期终止）后再 consume 循环 ✓（上面代码已是此序）；`Vec<&[u32]>` 的构造在循环内每轮新建。`consumed` 的 Vec 分配每轮一次——k 小可接受；若 profile 显示分配开销，Phase 2 换栈数组。

- [ ] **Step 7: 跑测试确认通过**

Run: `cargo test -p rustlucene-core 2>&1 | tail -3`
Expected: 全绿（281 + 2 = 283 passed）

- [ ] **Step 8: Commit**

```bash
git add crates/core/src/search/doc_iter.rs crates/core/src/search/block_tests.rs
git commit -m "$(cat <<'EOF'
feat(batch): 组合器覆写（一）Excluding/ConjOver/DisjOver 块代数

spec 2026-07-26 §4：Excl prohibited 块级跳过（替代逐候选 advance）、
ConjOver 成对 slice intersect、DisjOver k 路块归并。BlockCursor 游标
（1KB 堆块缓冲，18.7KB 子迭代器不挪动）。40 轮随机 + 边界形状对拍。

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```

---

