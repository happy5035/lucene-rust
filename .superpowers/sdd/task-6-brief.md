### Task 6: 组合器覆写（二）——Conjunction / Disjunction / RoaringAnd / RoaringOr

**Files:**
- Modify: `crates/core/src/search/doc_iter.rs`（PostingsIter :82-126 加 next_block；ConjunctionDocIter :130-233 / DisjunctionDocIter :237-321 覆写；DocSource :782-848 加 next_docs；RoaringAndDocIter :857-950 / RoaringOrDocIter :957-1012 覆写）
- Modify: `crates/core/src/search/block_tests.rs`（RoaringOr 对拍——DocSource::slice 路径可纯内存构造）

**Interfaces:**
- Consumes: Task 2 codec 批读（PostingsIter 包装）；Task 4 代数；Task 5 BlockCursor 模式
- Produces: 剩余四个组合器的 `next_block`——至此所有 SegmentDocIter 变体除 Phrase/Bitset（默认 fill，设计记录见 Task 3）外全部真块

- [ ] **Step 1: 写失败测试——RoaringOr slice 源对拍**

`block_tests.rs` 尾部追加：

```rust
use super::doc_iter::{DocSource, RoaringOrDocIter};

#[test]
fn roaring_or_slice_sources_block_vs_per_doc() {
    let mut lcg = Lcg(5678);
    for round in 0..30 {
        let k = 2 + (lcg.next_u32() % 4) as usize;
        let sets: Vec<Vec<u32>> = (0..k)
            .map(|_| lcg.doc_set(8_000, (lcg.next_u32() % 700) as usize))
            .collect();
        let mk = || {
            let sources: Vec<DocSource> = sets.iter().map(|s| DocSource::slice(s.clone())).collect();
            SegmentDocIter::RoaringOr(RoaringOrDocIter::new(sources))
        };
        let mut a = mk();
        let mut b = mk();
        assert_eq!(stream_block(&mut a), stream_per_doc(&mut b), "round={round}");
        assert_eq!(stream_block(&mut mk()), expect_disj(&sets));
    }
}
```

注：`RoaringAndDocIter` 需要 `FrozenBitmap` probes（codec 视图，测试不易构造）+ DocSource::Bitmap——其覆写逻辑（成对 intersect + contains 过滤）由 Task 8 的 1M roaring 电池（含 and 高/中桶）终验；slice 源路径与 RoaringOr 共享归并/交内核，已测。Conjunction/Disjunction（PostingsIter 子）需要真实索引——同样由 Task 8 的 PFOR 电池终验（and/or 桶逐 query 对账），加 Step 6 的 searcher 级集成测试覆盖。

- [ ] **Step 2: 跑测试确认当前状态**

Run: `cargo test -p rustlucene-core roaring_or 2>&1 | tail -5`
Expected: PASS（默认 fill 语义正确——回归守卫基线）

- [ ] **Step 3: PostingsIter::next_block**

`impl PostingsIter`（:86-126）尾部追加：

```rust
    /// 块级产出（spec Task 6）：codec 窗口批读。freqs 仅 decode_freqs
    /// 路径填充（needs_freq 构造）；no-freq enum 只填 docs。
    fn next_block(&mut self, out: &mut DocBlockBuf) -> io::Result<usize> {
        let n = match self {
            Self::Docs(d) => d.next_docs(&mut out.docs)?,
            Self::Freqs(f) => {
                if f.decodes_freqs() {
                    f.next_docs_and_freqs(&mut out.docs, &mut out.freqs)?
                } else {
                    f.next_docs(&mut out.docs)?
                }
            }
        };
        out.len = n;
        Ok(n)
    }
```

- [ ] **Step 4: ConjunctionDocIter::next_block**

`ConjunctionDocIter` 结构体（:130-134）加 `curs: Vec<BlockCursor>`，`new` 的两处构造（:153-157 与 :160-164）都补 `curs: sub.iter().map(|_| BlockCursor::new()).collect()`——注意 :153 的早退分支在 `sub` 已构造后，`curs` 表达式同。impl 尾部追加（PostingsIter 实现了本 crate的 DocIter？——**没有**：PostingsIter 只有固有方法。故游标 refill 不能走 `DocIter::next_block` 泛型）：

BlockCursor::refill 的 `<I: DocIter>` 约束不适用 PostingsIter。补一个固有方法到 BlockCursor：

```rust
impl BlockCursor {
    /// PostingsIter 专用 refill（PostingsIter 无 DocIter impl，固有方法分发）。
    fn refill_postings(&mut self, it: &mut PostingsIter) -> io::Result<bool> {
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
}
```

`position_seg` 的 PostingsIter 孪生版（PostingsIter 无 DocIter impl），与 `BlockCursor::refill_postings` 同区追加：

```rust
/// position_seg 的 PostingsIter 版（语义逐字一致）。
fn position_postings(
    curs: &mut BlockCursor,
    child: &mut PostingsIter,
    e: u32,
) -> io::Result<Option<bool>> {
    loop {
        while !curs.remaining().is_empty() && curs.remaining()[0] < e {
            curs.consume(1);
        }
        if let Some(head) = curs.remaining().first() {
            return Ok(Some(*head == e));
        }
        if !curs.refill_postings(child)? {
            return Ok(None);
        }
    }
}
```

`impl DocIter for ConjunctionDocIter`（:176-233）尾部追加（Task 5 ConjOver 覆写同构：二元 slice 快路径 + n 元逐元素定位）：

```rust
    fn next_block(&mut self, out: &mut DocBlockBuf) -> io::Result<usize> {
        let mut prod = 0;
        'blk: while prod < DOC_BLOCK {
            if self.curs[0].remaining().is_empty() && !self.curs[0].refill_postings(&mut self.sub[0])? {
                break 'blk;
            }
            if self.sub.len() == 2 {
                while self.curs[1].max_remaining().is_none_or(|hi| hi < self.curs[0].remaining()[0]) {
                    if !self.curs[1].refill_postings(&mut self.sub[1])? {
                        break 'blk;
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
                continue;
            }
            let c0_len = self.curs[0].remaining().len();
            let mut used = 0;
            for idx in 0..c0_len {
                let e = self.curs[0].buf.docs[self.curs[0].pos + idx];
                used += 1;
                let mut hit = true;
                for i in 1..self.sub.len() {
                    let (curs, sub) = (&mut self.curs, &mut self.sub);
                    match position_postings(&mut curs[i], &mut sub[i], e)? {
                        None => {
                            self.curs[0].consume(used);
                            self.doc = NO_MORE_DOCS;
                            out.len = prod;
                            return Ok(prod);
                        }
                        Some(false) => {
                            hit = false;
                            break;
                        }
                        Some(true) => {}
                    }
                }
                if hit {
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

- [ ] **Step 5: DisjunctionDocIter::next_block**

`DisjunctionDocIter` 结构体（:237-240）加 `curs: Vec<BlockCursor>`，`new`（:260）补 `curs: sub.iter().map(|_| BlockCursor::new()).collect()`。impl 尾部追加（Task 5 DisjOver 覆写同构，refill 改 `refill_postings`）：

```rust
    fn next_block(&mut self, out: &mut DocBlockBuf) -> io::Result<usize> {
        let mut prod = 0;
        while prod < DOC_BLOCK {
            let mut any = false;
            for i in 0..self.sub.len() {
                if self.curs[i].remaining().is_empty() && !self.curs[i].exhausted {
                    self.curs[i].refill_postings(&mut self.sub[i])?;
                }
                if !self.curs[i].remaining().is_empty() {
                    any = true;
                }
            }
            if !any {
                break;
            }
            let heads: Vec<&[u32]> = (0..self.sub.len()).map(|i| self.curs[i].remaining()).collect();
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
        self.doc = if prod > 0 { out.docs[prod - 1] as i32 } else { NO_MORE_DOCS };
        out.len = prod;
        Ok(prod)
    }
```

- [ ] **Step 6: DocSource::next_docs + RoaringOr / RoaringAnd 覆写**

`impl DocSource`（:793-848）尾部追加：

```rust
    /// 批量产出（spec Task 6）：先吐预拉的 current doc，再续批。
    /// 契约：next_docs 之后 current()/advance() 不得再调（块/逐 doc
    /// 模式不混用——driver 只选其一）。
    fn next_docs(&mut self, dst: &mut [u32]) -> usize {
        if dst.is_empty() {
            return 0;
        }
        let mut n = 0;
        if let Some(d) = self.current() {
            dst[0] = d;
            n = 1;
        }
        match self {
            DocSource::Bitmap { cur, doc } => {
                n += cur.next_many_to(&mut dst[n..]);
                *doc = None;
            }
            DocSource::Slice { docs, pos } => {
                let take = (docs.len() - *pos).min(dst.len() - n);
                dst[n..n + take].copy_from_slice(&docs[*pos..*pos + take]);
                *pos += take;
                n += take;
            }
        }
        n
    }
```

DocSource 块游标（复用 Box<[u32; DOC_BLOCK]> 轻量版——DocSource 子不带 freq）：

```rust
/// DocSource 专用块游标（RoaringAnd/Or 覆写用）。
struct SourceCursor {
    buf: Box<[u32; DOC_BLOCK]>,
    pos: usize,
    len: usize,
    exhausted: bool,
}

impl SourceCursor {
    fn new() -> SourceCursor {
        SourceCursor { buf: Box::new([0; DOC_BLOCK]), pos: 0, len: 0, exhausted: false }
    }
    fn refill(&mut self, src: &mut DocSource) -> bool {
        if self.exhausted {
            return false;
        }
        self.len = src.next_docs(&mut self.buf);
        self.pos = 0;
        if self.len == 0 {
            self.exhausted = true;
            return false;
        }
        true
    }
    fn remaining(&self) -> &[u32] {
        &self.buf[self.pos..self.len]
    }
    fn max_remaining(&self) -> Option<u32> {
        (self.pos < self.len).then(|| self.buf[self.len - 1])
    }
    fn consume(&mut self, n: usize) {
        self.pos += n;
    }
}
```

`RoaringOrDocIter` 结构体（:957-960）加 `curs: Vec<SourceCursor>`，`new`（:963-966）补 `curs: sources.iter().map(|_| SourceCursor::new()).collect()`。`impl DocIter for RoaringOrDocIter`（:969-1012）尾部追加（kway_union 归并，结构同 DisjOver，refill 改 SourceCursor::refill(&mut self.sources[i])）：

```rust
    fn next_block(&mut self, out: &mut DocBlockBuf) -> io::Result<usize> {
        let mut prod = 0;
        while prod < DOC_BLOCK {
            let mut any = false;
            for i in 0..self.sources.len() {
                if self.curs[i].remaining().is_empty() && !self.curs[i].exhausted {
                    self.curs[i].refill(&mut self.sources[i]);
                }
                if !self.curs[i].remaining().is_empty() {
                    any = true;
                }
            }
            if !any {
                break;
            }
            let heads: Vec<&[u32]> = (0..self.sources.len()).map(|i| self.curs[i].remaining()).collect();
            let mut consumed = vec![0usize; self.sources.len()];
            let n = kway_union(&heads, &mut consumed, &mut out.docs[prod..]);
            for i in 0..self.sources.len() {
                self.curs[i].consume(consumed[i]);
            }
            prod += n;
            if n == 0 {
                break;
            }
        }
        self.doc = if prod > 0 { out.docs[prod - 1] as i32 } else { NO_MORE_DOCS };
        out.len = prod;
        Ok(prod)
    }
```

`RoaringAndDocIter` 结构体（:857-861）加 `curs: Vec<SourceCursor>`，`new` 补初始化。`position_source`（无 io 版，随 SourceCursor 同区）：

```rust
/// position_seg 的 DocSource 版（无 io）。
fn position_source(curs: &mut SourceCursor, src: &mut DocSource, e: u32) -> Option<bool> {
    loop {
        while !curs.remaining().is_empty() && curs.remaining()[0] < e {
            curs.consume(1);
        }
        if let Some(head) = curs.remaining().first() {
            return Some(*head == e);
        }
        if !curs.refill(src) {
            return None;
        }
    }
}
```

覆写（二元 slice 快路径 + n 元逐元素定位，probes 点探过滤——contains 8.5–28.7 ns/探测，spec §4 记录）：

```rust
    fn next_block(&mut self, out: &mut DocBlockBuf) -> io::Result<usize> {
        let mut prod = 0;
        'blk: while prod < DOC_BLOCK {
            if self.curs[0].remaining().is_empty() && !self.curs[0].refill(&mut self.sources[0]) {
                break 'blk;
            }
            if self.sources.len() == 2 {
                while self.curs[1].max_remaining().is_none_or(|hi| hi < self.curs[0].remaining()[0]) {
                    if !self.curs[1].refill(&mut self.sources[1]) {
                        break 'blk;
                    }
                }
                let (ca, cb, n) = block_intersect(
                    self.curs[0].remaining(),
                    self.curs[1].remaining(),
                    &mut out.docs[prod..],
                );
                self.curs[0].consume(ca);
                self.curs[1].consume(cb);
                // probes 原地过滤（保序）
                let mut kept = 0;
                for j in 0..n {
                    let d = out.docs[prod + j];
                    if self.probes.iter().all(|p| p.contains(d)) {
                        out.docs[prod + kept] = d;
                        kept += 1;
                    }
                }
                prod += kept;
                continue;
            }
            let c0_len = self.curs[0].remaining().len();
            let mut used = 0;
            for idx in 0..c0_len {
                let e = self.curs[0].buf[self.curs[0].pos + idx];
                used += 1;
                let mut hit = true;
                for i in 1..self.sources.len() {
                    let (curs, srcs) = (&mut self.curs, &mut self.sources);
                    match position_source(&mut curs[i], &mut srcs[i], e) {
                        None => {
                            self.curs[0].consume(used);
                            self.doc = NO_MORE_DOCS;
                            out.len = prod;
                            return Ok(prod);
                        }
                        Some(false) => {
                            hit = false;
                            break;
                        }
                        Some(true) => {}
                    }
                }
                // probes 拒绝时不消费 child：e 留给下一轮 position 前向跳过
                if hit && self.probes.iter().all(|p| p.contains(e)) {
                    for i in 1..self.sources.len() {
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
        }
        out.len = prod;
        Ok(prod)
    }
```

- [ ] **Step 7: searcher 级集成对拍（含索引）**

`block_tests.rs` 尾部追加（用 crate 公开 API 建小索引；block 路径默认 ON 下 `Searcher::search` 结果 vs 显式 per-doc segment_iterator 驱动）：

```rust
use crate::{Document, FieldSpec, FieldValue, IndexWriter, IndexWriterConfig, Schema, Schema as _};
use super::query::Query;
use super::searcher::Searcher;
use codec_lucene9::directory::FSDirectory;

fn tiny_index(tag: &str, num_docs: u32, flush_every: u32) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rustlucene-block-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut s = Schema::new();
    s.add(FieldSpec::keyword("kw"));
    let mut config = IndexWriterConfig::default();
    config.max_buffered_docs = flush_every;
    let mut w = IndexWriter::create(&dir, s, config).unwrap();
    let terms = ["alpha", "beta", "gamma", "delta"];
    for i in 0..num_docs {
        let mut d = Document::new();
        d.add(FieldValue::keyword("kw", terms[(i % 4) as usize]));
        if i % 3 == 0 {
            d.add(FieldValue::keyword("kw", "extra"));
        }
        w.add_document(d).unwrap();
    }
    w.commit().unwrap();
    dir
}

#[test]
fn searcher_block_vs_explicit_per_doc_multi_segment() {
    // 300 docs，flush 每 100 → 3 段（跨段 doc_base 加宽覆盖）
    let dir = tiny_index("searcher", 300, 100);
    let fsdir = FSDirectory::open(&dir).unwrap();
    let mut searcher = Searcher::open(&fsdir).unwrap();
    assert!(searcher.segment_count() >= 2);

    let queries = vec![
        Query::term("kw", "alpha"),
        Query::and(vec![Query::term("kw", "alpha"), Query::term("kw", "extra")]),
        Query::or(vec![Query::term("kw", "alpha"), Query::term("kw", "beta")]),
        Query::bool(vec![
            (super::query::Occur::Must, Query::term("kw", "alpha")),
            (super::query::Occur::MustNot, Query::term("kw", "extra")),
        ]),
    ];
    for q in queries {
        // 实际：Searcher::search（block 路径，默认 ON）
        let mut c = CountCollector::default();
        searcher.search(&q, &mut c).unwrap();
        // 期望：显式 per-doc 逐段驱动（复刻旧 driver，含 matches）
        let mut expect = 0u64;
        {
            let mut s2 = Searcher::open(&fsdir).unwrap();
            for (_base, seg) in s2.leaves_for_test() {
                if let Some(mut it) = q.segment_iterator(seg, false).unwrap() {
                    loop {
                        if it.next_doc().unwrap() == NO_MORE_DOCS {
                            break;
                        }
                        if it.matches().unwrap() {
                            expect += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(c.count, expect, "query {q:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
```

注：`Reader::leaves()` 是 pub(crate)（reader.rs）——`Searcher` 无公开 leaves 访问。解法二选一（实现时按代码现状选）：(a) `Searcher` 加 `#[cfg(test)] pub fn leaves_for_test(&mut self)` 暴露 `self.reader.leaves()` 迭代；(b) 用 `Searcher::count`（per-doc 回退？count 也走 block……）——用 (a)。同时确认 `Query::term/and/or/bool` 构造器签名（query.rs 公开 API）与 `FieldValue::keyword` 存在——按编译器报错修正 import/签名，语义不变。

- [ ] **Step 8: 跑测试确认通过**

Run: `cargo test -p rustlucene-core 2>&1 | tail -3`
Expected: 全绿（283 + 2 = 285 passed）

Run: `cargo test --workspace 2>&1 | tail -3`
Expected: 全绿（287 passed：core 102 + codec 185... 以实际基线为准，0 failed）

- [ ] **Step 9: Commit**

```bash
git add crates/core/src/search/doc_iter.rs crates/core/src/search/block_tests.rs crates/core/src/search/searcher.rs
git commit -m "$(cat <<'EOF'
feat(batch): 组合器覆写（二）Conjunction/Disjunction/RoaringAnd/RoaringOr

spec 2026-07-26 Task 6：PostingsIter::next_block 接 codec 批读；
DocSource::next_docs + SourceCursor 供 Roaring 组合器块归并/交 +
probes contains 过滤。至此除 Phrase/Bitset（默认 fill，设计记录）外
全部 SegmentDocIter 变体真块。searcher 级多段集成对拍。

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```

---

