# M7 两阶段迭代协议 + Bool count fold + Top-N 提前终止 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给搜索读路径加两阶段迭代协议（Phrase 拆分 + bitmap 候选快路径）、通用 Bool count 的 roaring fold、Top-N 提前终止（count/topN 分离），并按 bench 门槛决定多子句 OR 堆化。

**Architecture:** 全部改动在 `crates/core/src/search/`（+ codec-lucene9 的 roaring 模块加 4 个容器级 fold 原语）。协议以 `DocIter::matches()` 默认实现为核心——只有 Phrase 两阶段，组合器在内部吸收子句 confirmation；count fold 递归物化任意查询为 `MaterializedBitmap` 后 and/or/andnot；top_docs 复用统一的段级 count 快路径入口实现提前终止。无格式改动、无索引兼容性影响。

**Tech Stack:** Rust workspace（`rustlucene-core` + `codec-lucene9`），croaring 2.7（已封装在 codec 内，core 无 croaring 依赖）。

**Spec:** `docs/superpowers/specs/2026-07-25-rust-search-m7-twophase-fold-topn-design.md`（用户批准，方案一 A+B+C+D）。

## Global Constraints

- **core 不得依赖 croaring**：fold 原语一律经 `codec_lucene9::roaring::MaterializedBitmap` 表面（M5 关键设计事实 7）。
- **语义不变量**：每个任务交付后，同查询同索引的 count/top_docs/迭代序列与改动前**完全一致**（除非任务明确是性能路径切换，结果仍须一致）。
- **A/B 纪律**：`RL_BITMAP=0` 下所有路径回落 postings，结果一致（既有 kill switch，不得破坏）。
- **不新增第三方依赖**；codec 测试 `cargo test -p codec-lucene9 --lib`，core 测试 `RUST_MIN_STACK=4M cargo test -p rustlucene-core --lib`（嵌套 Bool 递归需要 4M 栈）。
- **`DocIter` 协议新不变量**：任何驱动方拿到 `next_doc()`/`advance()` 返回的候选后必须调 `matches()`；返回 false 则以 `next_doc()` 推进（候选已消费）。`matches()` 对同一候选 doc 至多调用一次。
- 提交信息沿用项目惯例：`feat: M7 ...` / `test: M7 ...`，中文要点。
- 测试代码惯例（`crates/core/src/search/mod.rs` 的 `#[cfg(test)] mod tests`）：`temp_dir(tag)`、`schema()` / `schema_pos()`、`doc()` / `pos_doc()`、`FSDirectory::open` + `Searcher::open`、结尾 `fs::remove_dir_all`。

---

### Task 1: 两阶段协议骨架——`DocIter::matches()` + 组合器吸收 + 驱动调用

**Files:**
- Modify: `crates/core/src/search/doc_iter.rs`（trait 定义 :17-34；ConjOverDocIter :936-1019；DisjOverDocIter :1025-1088；ExcludingDocIter :1093-1152；SegmentDocIter DocIter impl :1175-1238）
- Modify: `crates/core/src/search/searcher.rs:44-51`（search 驱动循环）
- Modify: `crates/core/src/search/query.rs:568-579`（drive_count）

**Interfaces:**
- Consumes: 现有 `DocIter` trait 与组合器。
- Produces: `DocIter::matches(&mut self) -> io::Result<bool>`（默认 `Ok(true)`）；`SegmentDocIter::matches()` 分派（本任务只有 Phrase 覆盖、且行为与现状一致——见 Step 2 注）。后续所有任务依赖此协议。

本任务是行为保持的协议引入：所有迭代器仍为单阶段语义（Phrase 的 `matches()` 在本任务**不**改构造，直接覆盖为对当前 doc 重跑位置验证——由于 PhraseDocIter 尚未拆分，其 `next_doc` 已做验证，`matches()` 返回 `Ok(true)` 即可，**本任务不给 PhraseDocIter 加覆盖**）。因此全部既有测试应原样通过。

- [ ] **Step 1: trait 加默认 `matches()` + SegmentDocIter 分派**

`crates/core/src/search/doc_iter.rs` 的 `DocIter` trait（:31 `fn freq` 之后）加：

```rust
    /// 两阶段确认（M7 §2.1，Lucene TwoPhaseIterator.matches）：对
    /// next_doc/advance 返回的当前候选做昂贵验证；默认 Ok(true) = 单阶段
    /// 迭代器。返回 false 后调用方以 next_doc() 推进（候选已消费）；
    /// 对同一候选 doc 至多调用一次。
    fn matches(&mut self) -> io::Result<bool> {
        Ok(true)
    }
```

`impl DocIter for SegmentDocIter`（:1230 `fn freq` 之后）加：

```rust
    fn matches(&mut self) -> io::Result<bool> {
        match self {
            Self::Phrase(p) => p.matches(),
            _ => Ok(true),
        }
    }
```

- [ ] **Step 2: ConjOverDocIter 对齐后吸收子句 confirmation**

`ConjOverDocIter::next_doc`（doc_iter.rs:979-1005）的 `if matched { ... }` 分支替换为：

```rust
            if matched {
                // M7 §2.3：approximation 对齐后逐个 confirmation（Lucene
                // ConjunctionScorer 同款）；任一 false → 推进停在
                // candidate 的子句（含已确认的）后重新对齐。
                let mut all_match = true;
                for s in &mut self.sub {
                    if !s.matches()? {
                        all_match = false;
                        break;
                    }
                }
                if all_match {
                    self.doc = candidate;
                    return Ok(candidate);
                }
                for s in &mut self.sub {
                    if s.doc_id() == candidate && s.next_doc()? == NO_MORE_DOCS {
                        self.doc = NO_MORE_DOCS;
                        return Ok(NO_MORE_DOCS);
                    }
                }
            }
```

- [ ] **Step 3: DisjOverDocIter 命中前吸收子句 confirmation**

`DisjOverDocIter::next_doc`（doc_iter.rs:1045-1065）整体替换为：

```rust
    fn next_doc(&mut self) -> io::Result<i32> {
        if self.doc == NO_MORE_DOCS {
            return Ok(NO_MORE_DOCS);
        }
        if self.doc >= 0 {
            for s in &mut self.sub {
                if s.doc_id() == self.doc {
                    s.next_doc()?;
                }
            }
        }
        loop {
            let mut best = NO_MORE_DOCS;
            for s in &self.sub {
                let d = s.doc_id();
                if d != NO_MORE_DOCS && d < best {
                    best = d;
                }
            }
            if best == NO_MORE_DOCS {
                self.doc = NO_MORE_DOCS;
                return Ok(NO_MORE_DOCS);
            }
            // M7 §2.3：对停在 best 的子句逐个 confirmation；至少一个
            // true → 命中（短路，省掉其余子句的确认成本）。
            let mut any = false;
            for s in &mut self.sub {
                if s.doc_id() == best && s.matches()? {
                    any = true;
                    break;
                }
            }
            if any {
                self.doc = best;
                return Ok(best);
            }
            for s in &mut self.sub {
                if s.doc_id() == best {
                    s.next_doc()?;
                }
            }
        }
    }
```

- [ ] **Step 4: ExcludingDocIter 排除检查后吸收 main confirmation**

`ExcludingDocIter::next_non_excluded`（doc_iter.rs:1109-1125）的 `if self.prohibited.advance(d)? != d {` 一行改为短路且（排除命中时**不**调 main.matches()，省位置解码）：

```rust
            if self.prohibited.advance(d)? != d && self.main.matches()? {
                self.doc = d;
                return Ok(d);
            }
```

- [ ] **Step 5: 两处顶层驱动调用 matches()**

`crates/core/src/search/searcher.rs:44-51` 驱动循环改为：

```rust
            loop {
                let doc = iter.next_doc()?;
                if doc == NO_MORE_DOCS {
                    break;
                }
                if !iter.matches()? {
                    continue;
                }
                let freq = if needs_freq { iter.freq() } else { 1 };
                collector.collect(doc_base + doc, freq);
            }
```

`crates/core/src/search/query.rs` 的 `drive_count`（:568-579）改为：

```rust
fn drive_count(it: Option<SegmentDocIter>) -> io::Result<u64> {
    let mut n = 0u64;
    if let Some(mut it) = it {
        loop {
            if it.next_doc()? == NO_MORE_DOCS {
                break;
            }
            if !it.matches()? {
                continue;
            }
            n += 1;
        }
    }
    Ok(n)
}
```

- [ ] **Step 6: 全量测试确认行为保持**

Run: `RUST_MIN_STACK=4M cargo test -p rustlucene-core --lib 2>&1 | tail -3`
Expected: `test result: ok. 76 passed`（全部既有测试原样通过——本任务无行为变化）

- [ ] **Step 7: Commit**

```bash
git add crates/core/src/search/doc_iter.rs crates/core/src/search/searcher.rs crates/core/src/search/query.rs
git commit -m "feat: M7 T-A1 两阶段协议骨架——DocIter::matches() 默认实现 + ConjOver/DisjOver/Excluding 吸收子句 confirmation + 顶层驱动调用（行为保持）"
```

---

### Task 2: PhraseDocIter 拆分——postings approximation + matches() 位置确认

**Files:**
- Modify: `crates/core/src/search/doc_iter.rs:360-516`（PhraseDocIter 全体）
- Test: `crates/core/src/search/mod.rs`（`#[cfg(test)] mod tests`，新增 `phrase_nested_bool_twophase_equivalence`）

**Interfaces:**
- Consumes: Task 1 的 `DocIter::matches()` 协议；`schema_pos()` / `pos_doc(level, tid, message)` 测试辅助（mod.rs:447/:455）。
- Produces: `PhraseDocIter`：`next_doc`/`advance` 只做 postings 合取（approximation，不解码位置）；`matches()` 对当前候选做位置验证。结果与拆分前逐 doc 一致。Task 3 在其构造器里加 bitmap 快路径。

注意：本任务的等价测试在旧代码上**也会通过**（语义不变量钉死），先写测试确认绿、再拆分、再确认绿——拆分是行为保持重构，测试防回归。

- [ ] **Step 1: 写等价钉死测试（先在旧实现上跑绿）**

`crates/core/src/search/mod.rs` tests 模块末尾加：

```rust
    /// M7 T-A2：phrase 两阶段拆分的语义钉死——单用 / 嵌套 MUST / 嵌套
    /// SHOULD / MUST_NOT 组合的结果集。语料含 co-occur 非相邻 doc
    /// （approximation 命中但 confirmation 拒绝），覆盖组合器吸收路径。
    #[test]
    fn phrase_nested_bool_twophase_equivalence() {
        let root = temp_dir("twophase");
        let mut w =
            IndexWriter::create(&root, schema_pos(), IndexWriterConfig::default()).unwrap();
        // t1 命中；t2 非相邻；t3 逆序；t4 命中(WARN)；t5 alpha@1+beta@2 命中；t6 命中(WARN)
        let docs = [
            ("INFO", "t1", "alpha beta gamma"),
            ("INFO", "t2", "alpha gamma beta"),
            ("WARN", "t3", "beta alpha gamma"),
            ("WARN", "t4", "alpha beta"),
            ("INFO", "t5", "alpha alpha beta"),
            ("WARN", "t6", "gamma alpha beta delta"),
        ];
        for (l, t, m) in docs {
            w.add_document(pos_doc(l, t, m)).unwrap();
        }
        w.commit().unwrap();
        drop(w);
        let dir = FSDirectory::open(&root).unwrap();
        let mut s = Searcher::open(&dir).unwrap();
        let phrase = Query::phrase("message", &["alpha", "beta"]);
        assert_eq!(s.count(&phrase).unwrap(), 4); // t1,t4,t5,t6
        // MUST[phrase, level=INFO] → t1,t5（跨字段合取，confirmation 后于对齐）
        let q = Query::bool(vec![
            (Occur::Must, phrase.clone()),
            (Occur::Must, Query::term("level", "INFO")),
        ]);
        let (total, docs) = s.top_docs(&q, 100).unwrap();
        assert_eq!(total, 2);
        assert_eq!(docs.len(), 2);
        // SHOULD[phrase, tid=t2] → phrase 4 + t2 = 5
        let q = Query::bool(vec![
            (Occur::Should, phrase.clone()),
            (Occur::Should, Query::term("tid", "t2")),
        ]);
        assert_eq!(s.count(&q).unwrap(), 5);
        // MUST phrase + MUST_NOT level=INFO → t4,t6
        let q = Query::bool(vec![
            (Occur::Must, phrase.clone()),
            (Occur::MustNot, Query::term("level", "INFO")),
        ]);
        assert_eq!(s.count(&q).unwrap(), 2);
        fs::remove_dir_all(&root).unwrap();
    }
```

Run: `RUST_MIN_STACK=4M cargo test -p rustlucene-core --lib phrase_nested_bool_twophase 2>&1 | tail -3`
Expected: PASS（旧实现即满足——这是钉死测试）

- [ ] **Step 2: 拆分 PhraseDocIter**

`crates/core/src/search/doc_iter.rs` 的 `impl DocIter for PhraseDocIter`（:457-516）替换为：

```rust
impl PhraseDocIter {
    /// approximation 推进（M7 §2.2）：只做 postings 合取对齐
    /// （ConjunctionDISI 舞蹈），**不解码位置**——候选直接返回，位置
    /// 验证推迟到 matches()。
    fn next_candidate(&mut self) -> io::Result<i32> {
        if self.doc >= 0 {
            for o in &mut self.occ {
                if o.en.doc_id() == self.doc && o.en.next_doc()? == NO_MORE_DOCS {
                    self.doc = NO_MORE_DOCS;
                    return Ok(NO_MORE_DOCS);
                }
            }
        }
        loop {
            let candidate = self.occ[self.lead].en.doc_id();
            if candidate == NO_MORE_DOCS {
                self.doc = NO_MORE_DOCS;
                return Ok(NO_MORE_DOCS);
            }
            let mut matched = true;
            for i in 0..self.occ.len() {
                if i == self.lead {
                    continue;
                }
                let d = self.occ[i].en.advance(candidate)?;
                if d == NO_MORE_DOCS {
                    self.doc = NO_MORE_DOCS;
                    return Ok(NO_MORE_DOCS);
                }
                if d > candidate {
                    self.lead = i;
                    matched = false;
                    break;
                }
            }
            if matched {
                self.doc = candidate;
                return Ok(candidate);
            }
        }
    }
}

impl DocIter for PhraseDocIter {
    fn doc_id(&self) -> i32 {
        self.doc
    }

    fn next_doc(&mut self) -> io::Result<i32> {
        if self.doc == NO_MORE_DOCS {
            return Ok(NO_MORE_DOCS);
        }
        self.next_candidate()
    }

    /// 两阶段确认（M7 §2.2）：对当前候选解码位置并验证
    /// （ExactPhraseMatcher :138-167，逻辑从旧 next_doc 原样搬入）。
    fn matches(&mut self) -> io::Result<bool> {
        debug_assert!(self.doc >= 0 && self.doc != NO_MORE_DOCS);
        self.positions_match()
    }
    // advance: trait default（线性 next_candidate 循环，approximation
    // 语义）；freq: 1（ConstantScore，trait default）。
}
```

（旧 `next_doc` 里的 `positions_match` 调用与"no positional match: move every occurrence past it"块删除——确认失败的重试现在由**驱动方**负责：matches() false → next_doc() 推进。）

- [ ] **Step 3: 跑钉死测试 + 全量**

Run: `RUST_MIN_STACK=4M cargo test -p rustlucene-core --lib 2>&1 | tail -3`
Expected: `test result: ok. 77 passed`（76 既有 + 1 新增，全绿）

- [ ] **Step 4: Commit**

```bash
git add crates/core/src/search/doc_iter.rs crates/core/src/search/mod.rs
git commit -m "feat: M7 T-A2 PhraseDocIter 两阶段拆分——next_doc 只做 postings 合取 approximation，matches() 位置确认（嵌套 Bool 下被拒绝候选不再白解码位置）"
```

---

### Task 3: Phrase bitmap 候选快路径（全 term 有内联 bitmap 时 roaring AND approximation）

**Files:**
- Modify: `crates/core/src/search/doc_iter.rs:372-455`（PhraseDocIter 构造器 + 新增 PhraseApprox 枚举 + next_doc/matches 分支）
- Test: `crates/core/src/search/mod.rs`（新增 `phrase_bitmap_approx_equivalence`）

**Interfaces:**
- Consumes: Task 2 拆分后的 PhraseDocIter；`SegmentReader::open_term_bitmap(&self, entry: &TermEntry) -> io::Result<Option<FrozenBitmap>>`（segment_reader.rs:89）；`codec_lucene9::roaring::{FrozenBitmap, intersect_docs}`（frozen.rs:92/:176）。
- Produces: `PhraseDocIter` 构造时全 term 有 bitmap → approximation = roaring AND 物化 doc 序列；否则回落 Task 2 的 postings 合取。`matches()` 对两种 approximation 通用（内部 advance 对齐 enum）。Task 5 的 fold 叶子与 Task 6 不受影响。

- [ ] **Step 1: 写等价测试（bitmap on/off 两索引结果一致）**

`crates/core/src/search/mod.rs` tests 模块末尾加：

```rust
    /// M7 T-A3：phrase bitmap 候选快路径等价——hot/warm df=5000 ≥ 4096
    /// （bitmap 索引上两者都有内联 bitmap → roaring AND approximation），
    /// 与 bitmap off 索引（postings 合取 approximation）逐位一致。
    fn write_phrase_bitmap_corpus(root: &std::path::Path, bitmap: bool) {
        let mut cfg = IndexWriterConfig::default();
        cfg.bitmap = bitmap;
        let mut w = IndexWriter::create(root, schema_pos(), cfg).unwrap();
        for i in 0..5000u32 {
            let msg = if i % 3 == 0 { "hot warm" } else { "hot x warm" };
            w.add_document(pos_doc("INFO", &format!("tid-{i}"), msg)).unwrap();
        }
        w.commit().unwrap();
        drop(w);
    }

    #[test]
    fn phrase_bitmap_approx_equivalence() {
        let root_off = temp_dir("phbmoff");
        let root_on = temp_dir("phbmon");
        write_phrase_bitmap_corpus(&root_off, false);
        write_phrase_bitmap_corpus(&root_on, true);
        let dir_off = FSDirectory::open(&root_off).unwrap();
        let mut s_off = Searcher::open(&dir_off).unwrap();
        let dir_on = FSDirectory::open(&root_on).unwrap();
        let mut s_on = Searcher::open(&dir_on).unwrap();
        let battery: Vec<Query> = vec![
            Query::phrase("message", &["hot", "warm"]),          // 1667（i%3==0）
            Query::phrase("message", &["hot", "x"]),             // 3333
            Query::bool(vec![
                (Occur::Must, Query::phrase("message", &["hot", "warm"])),
                (Occur::Must, Query::term("level", "INFO")),
            ]),
            Query::bool(vec![
                (Occur::Must, Query::phrase("message", &["hot", "warm"])),
                (Occur::MustNot, Query::term("tid", "tid-7")),
            ]),
        ];
        for q in &battery {
            let (a_total, a_docs) = s_off.top_docs(q, 6000).unwrap();
            let (b_total, b_docs) = s_on.top_docs(q, 6000).unwrap();
            assert_eq!((a_total, a_docs), (b_total, b_docs), "top_docs {q:?}");
            assert_eq!(s_off.count(q).unwrap(), s_on.count(q).unwrap(), "count {q:?}");
        }
        assert_eq!(s_on.count(&Query::phrase("message", &["hot", "warm"])).unwrap(), 1667);
        assert_eq!(s_on.count(&Query::phrase("message", &["hot", "x"])).unwrap(), 3333);
        fs::remove_dir_all(&root_off).unwrap();
        fs::remove_dir_all(&root_on).unwrap();
    }
```

Run: `RUST_MIN_STACK=4M cargo test -p rustlucene-core --lib phrase_bitmap_approx 2>&1 | tail -3`
Expected: PASS（旧实现即等价——钉死测试，防快路径引入偏差）

- [ ] **Step 2: 加 PhraseApprox 枚举 + 构造器快路径**

`crates/core/src/search/doc_iter.rs`，`struct Occurrence` 之前加：

```rust
/// phrase approximation 源（M7 §2.2）：全 term 有内联 bitmap 时 roaring
/// AND 物化候选序列（µs 级，比 PFOR 合取快）；否则 postings 合取舞蹈。
enum PhraseApprox {
    Postings,
    Bitmap { docs: Vec<u32>, cursor: usize },
}
```

`PhraseDocIter` struct 加字段：

```rust
pub struct PhraseDocIter {
    occ: Vec<Occurrence>, // df-ascending (conjunction cost order)
    approx: PhraseApprox,
    doc: i32,
    lead: usize,
}
```

`PhraseDocIter::new`（:384-427）改为（保留 field/positions 检查与 df 排序；entry 暂存以开 bitmap）：

```rust
        let mut sought: Vec<(u32, u32, TermEntry)> = Vec::with_capacity(terms.len());
        for (i, t) in terms.iter().enumerate() {
            let Some((_, entry)) = seg.seek_term(field, t)? else {
                return Ok(None); // absent term: no hits (PhraseWeight null scorer)
            };
            sought.push((entry.doc_freq, i as u32, entry));
        }
        sought.sort_by_key(|(df, _, _)| *df);
        // M7 §2.2 bitmap 候选快路径：全部 term 有内联 bitmap → roaring
        // AND 物化候选序列；任一缺失 → postings 合取 approximation。
        let mut views: Vec<FrozenBitmap> = Vec::with_capacity(sought.len());
        let mut all_bitmap = true;
        for (_, _, entry) in &sought {
            match seg.open_term_bitmap(entry)? {
                Some(v) => views.push(v),
                None => {
                    all_bitmap = false;
                    break;
                }
            }
        }
        let approx = if all_bitmap {
            let refs: Vec<&FrozenBitmap> = views.iter().collect();
            PhraseApprox::Bitmap {
                docs: codec_lucene9::roaring::intersect_docs(&refs),
                cursor: 0,
            }
        } else {
            PhraseApprox::Postings
        };
        let mut occ: Vec<Occurrence> = Vec::with_capacity(sought.len());
        for (_, offset, entry) in sought {
            occ.push(Occurrence {
                en: seg.positions_enum(&entry)?,
                offset,
            });
        }
        Ok(Some(PhraseDocIter {
            occ,
            approx,
            doc: -1,
            lead: 0,
        }))
```

注意：`intersect_docs(&[])` 有 `debug_assert!(!bitmaps.is_empty())`——terms.len() ≥ 2（单 term 在 query.rs:248-254 已退化 Term 路径），views 非空，安全。

- [ ] **Step 3: next_doc/matches 分源**

`next_doc`（Task 2 版）改为：

```rust
    fn next_doc(&mut self) -> io::Result<i32> {
        if self.doc == NO_MORE_DOCS {
            return Ok(NO_MORE_DOCS);
        }
        match &mut self.approx {
            PhraseApprox::Postings => self.next_candidate(),
            PhraseApprox::Bitmap { docs, cursor } => {
                if self.doc >= 0 {
                    *cursor += 1;
                }
                match docs.get(*cursor) {
                    Some(&d) => {
                        self.doc = d as i32;
                        Ok(self.doc)
                    }
                    None => {
                        self.doc = NO_MORE_DOCS;
                        Ok(NO_MORE_DOCS)
                    }
                }
            }
        }
    }
```

`matches()`（Task 2 版）改为（bitmap 源时 enum 未定位到候选，先 advance 对齐；postings 源时 enum 已在 doc，`doc_id()==self.doc`，零成本）：

```rust
    fn matches(&mut self) -> io::Result<bool> {
        debug_assert!(self.doc >= 0 && self.doc != NO_MORE_DOCS);
        for o in &mut self.occ {
            if o.en.doc_id() != self.doc {
                // bitmap approximation：positions enum 尚未定位——advance
                // 对齐（跳表，非逐 doc）；近似集 ⊆ 各 term doc 集，必命中。
                let d = o.en.advance(self.doc)?;
                if d != self.doc {
                    return Ok(false); // 防御（不变量破坏时宁可漏不可错）
                }
            }
        }
        self.positions_match()
    }
```

`next_candidate` 顶部加近似源断言（bitmap 源不应对齐 enum——它们可能不在 doc 上）：

```rust
    fn next_candidate(&mut self) -> io::Result<i32> {
        debug_assert!(matches!(self.approx, PhraseApprox::Postings));
        // ……其余不变
```

- [ ] **Step 4: 全量测试**

Run: `RUST_MIN_STACK=4M cargo test -p rustlucene-core --lib 2>&1 | tail -3`
Expected: `test result: ok. 79 passed`（77 + 2 新增，全绿）

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/search/doc_iter.rs crates/core/src/search/mod.rs
git commit -m "feat: M7 T-A3 phrase bitmap 候选快路径——全 term 有内联 bitmap 时 roaring AND approximation，位置验证只对候选 doc"
```

---

### Task 4: codec roaring fold 原语（MaterializedBitmap set ops + full + FrozenBitmap::to_materialized）

**Files:**
- Modify: `crates/codec-lucene9/src/roaring/materialized.rs`（加 from_bitmap/and/or/andnot/full + 测试）
- Modify: `crates/codec-lucene9/src/roaring/frozen.rs`（加 to_materialized + 测试）

**Interfaces:**
- Consumes: croaring 2.7 `Bitmap::{and, or, andnot}`、`Bitmap::create` + `add_range`、`BitmapView::to_bitmap`（frozen.rs:178 已用）。
- Produces（Task 5 依赖的精确签名）:
  - `MaterializedBitmap::of(sorted_dedup_docs: &[u32]) -> MaterializedBitmap`（已有）
  - `MaterializedBitmap::full(max_doc: u32) -> MaterializedBitmap`
  - `MaterializedBitmap::{and, or, andnot}(&self, other: &MaterializedBitmap) -> MaterializedBitmap`
  - `MaterializedBitmap::{cardinality, docs_from}`（已有）
  - `FrozenBitmap::to_materialized(&self) -> MaterializedBitmap`

- [ ] **Step 1: 写 codec 失败测试**

`crates/codec-lucene9/src/roaring/materialized.rs` 的 `#[cfg(test)] mod tests` 加：

```rust
    #[test]
    fn set_ops_full_and_andnot() {
        let a = MaterializedBitmap::of(&[1, 2, 3, 5, 8]);
        let b = MaterializedBitmap::of(&[2, 3, 4, 8, 13]);
        let inter = a.and(&b);
        assert_eq!(inter.cardinality(), 3); // {2,3,8}
        let uni = a.or(&b);
        assert_eq!(uni.cardinality(), 7); // {1,2,3,4,5,8,13}
        let diff = a.andnot(&b);
        assert_eq!(diff.cardinality(), 2); // {1,5}
        let mut buf = [0u32; 8];
        let n = diff.docs_from(0, &mut buf);
        assert_eq!(&buf[..n], [1, 5]);
        // full：0..max_doc 全位
        let full = MaterializedBitmap::full(100);
        assert_eq!(full.cardinality(), 100);
        let n = full.docs_from(98, &mut buf);
        assert_eq!(&buf[..n], [98, 99]);
        // 与空集运算
        let empty = MaterializedBitmap::of(&[]);
        assert_eq!(a.and(&empty).cardinality(), 0);
        assert_eq!(a.or(&empty).cardinality(), 5);
        assert_eq!(a.andnot(&empty).cardinality(), 5);
        assert_eq!(empty.andnot(&a).cardinality(), 0);
    }
```

`crates/codec-lucene9/src/roaring/frozen.rs` 的 `#[cfg(test)] mod tests` 加（该模块已 `use croaring::Bitmap;`，仿照既有 frozen 序列化测试的构造方式——参照文件内既有测试如何造 FrozenBitmap；若既有测试经 `crate::roaring::write_term_bitmap` + `parse_region` 构造则沿用）：

```rust
    #[test]
    fn to_materialized_container_copy() {
        let docs: Vec<u32> = (0..9000u32).map(|i| i * 2).collect();
        let mut buf = Vec::new();
        crate::roaring::write_term_bitmap(&mut buf, &docs).unwrap();
        let fb = parse_region(&buf, docs.len() as u32).expect("valid region");
        let m = fb.to_materialized();
        assert_eq!(m.cardinality(), docs.len() as u64);
        let mut out = [0u32; 1024];
        let mut got = Vec::new();
        let mut from = 0;
        loop {
            let n = m.docs_from(from, &mut out);
            if n == 0 {
                break;
            }
            got.extend_from_slice(&out[..n]);
            from = got.last().unwrap() + 1;
        }
        assert_eq!(got, docs);
    }
```

Run: `cargo test -p codec-lucene9 --lib roaring 2>&1 | tail -3`
Expected: FAIL（`full`/`and`/`or`/`andnot`/`to_materialized` 未定义，编译错误）

- [ ] **Step 2: 实现 materialized.rs 原语**

`MaterializedBitmap` impl 加：

```rust
    /// Owned 构造（codec 内部：frozen 容器级拷贝与 set ops 的出口）。
    pub(crate) fn from_bitmap(bm: croaring::Bitmap) -> MaterializedBitmap {
        MaterializedBitmap { bm }
    }

    /// 全位 bitmap [0, max_doc)（M7 §3.1：Bool 内嵌纯 MUST_NOT 子树的
    /// MatchAll 正集防御）。
    pub fn full(max_doc: u32) -> MaterializedBitmap {
        let mut bm = croaring::Bitmap::create();
        bm.add_range(0..max_doc);
        MaterializedBitmap { bm }
    }

    /// 容器级交（M7 §3.1 fold 原语）：新分配 owned 结果。
    pub fn and(&self, other: &MaterializedBitmap) -> MaterializedBitmap {
        MaterializedBitmap {
            bm: self.bm.and(&other.bm),
        }
    }

    /// 容器级并。
    pub fn or(&self, other: &MaterializedBitmap) -> MaterializedBitmap {
        MaterializedBitmap {
            bm: self.bm.or(&other.bm),
        }
    }

    /// 容器级差（MUST_NOT 排除）。
    pub fn andnot(&self, other: &MaterializedBitmap) -> MaterializedBitmap {
        MaterializedBitmap {
            bm: self.bm.andnot(&other.bm),
        }
    }
```

- [ ] **Step 3: 实现 FrozenBitmap::to_materialized**

`crates/codec-lucene9/src/roaring/frozen.rs` 的 `FrozenBitmap` impl 加（顶部加 `use super::materialized::MaterializedBitmap;`）：

```rust
    /// 容器级拷贝为 owned bitmap（M7 §3.1：fold 叶子的零迭代转换——
    /// view().to_bitmap() 按容器克隆，不逐 doc）。
    pub fn to_materialized(&self) -> MaterializedBitmap {
        MaterializedBitmap::from_bitmap(self.view().to_bitmap())
    }
```

- [ ] **Step 4: 测试 + 全量 codec 测试**

Run: `cargo test -p codec-lucene9 --lib 2>&1 | tail -3`
Expected: `test result: ok. 182 passed`（180 既有 + 2 新增；若既有数不同以实际为准，全绿即可）

- [ ] **Step 5: Commit**

```bash
git add crates/codec-lucene9/src/roaring/materialized.rs crates/codec-lucene9/src/roaring/frozen.rs
git commit -m "feat: M7 T-B1 codec roaring fold 原语——MaterializedBitmap and/or/andnot/full + FrozenBitmap::to_materialized 容器级拷贝（各含测试）"
```

---

### Task 5: Bool count bitmap fold（materialize_bool_bitmap + 成本护栏）

**Files:**
- Modify: `crates/core/src/search/query.rs`（bool_segment_count :514-540 插入 fold；新增 materialize_query_bitmap / materialize_bool_bitmap / MatOutcome / FOLD_COST_FACTOR）
- Test: `crates/core/src/search/mod.rs`（新增 `bool_count_fold_matches_drive` + `fold_cost_guard_triggers`）

**Interfaces:**
- Consumes: Task 4 的 `MaterializedBitmap::{of, full, and, or, andnot, cardinality}`、`FrozenBitmap::to_materialized`；`multi_term::{collect_direct, collect_prefix, collect_wildcard, WildcardPattern, for_each_doc}`；`roaring_exec::collect_bool_entries`；`point_range_bitmap`（query.rs:324）。
- Produces: `pub(crate) fn materialize_bool_bitmap(seg: &mut SegmentReader, clauses: &[(Occur, Query)], budget: u64, cost: &mut u64) -> io::Result<MatOutcome>`；`pub(crate) enum MatOutcome { Hits(MaterializedBitmap), OverBudget }`；`const FOLD_COST_FACTOR: u64 = 4`。Task 6 的 fast count 复用。`bool_segment_count` 在拍平/纯 MUST_NOT 之后、drive_count 之前插入 fold。

- [ ] **Step 1: 写等价钉死测试（独立迭代参照）**

`crates/core/src/search/mod.rs` tests 模块加（顶部 `use codec_lucene9::postings_read::NO_MORE_DOCS;` 若尚未引入则加在 helper 内联）：

```rust
    /// M7 T-B/T-D 的独立参照：不经任何 count 快路径，纯迭代 + matches
    /// 驱动计数（永远正确，用于钉死各 count 快路径的等价性）。
    fn drive_count_reference(dir: &FSDirectory, q: &Query) -> u64 {
        use codec_lucene9::postings_read::NO_MORE_DOCS;
        let mut reader = Reader::open(dir).unwrap();
        let mut n = 0u64;
        for (_b, seg) in reader.leaves() {
            if let Some(mut it) = q.segment_iterator(seg, false).unwrap() {
                loop {
                    if it.next_doc().unwrap() == NO_MORE_DOCS {
                        break;
                    }
                    if !it.matches().unwrap() {
                        continue;
                    }
                    n += 1;
                }
            }
        }
        n
    }

    /// M7 T-B：通用 Bool 形状的 count fold 与逐 doc 迭代完全一致
    /// （跨字段 / MUST_NOT / 嵌套 / phrase 叶子 / 多 term 叶子）。
    #[test]
    fn bool_count_fold_matches_drive() {
        let root = temp_dir("fold");
        write_phrase_bitmap_corpus(&root, true); // Task 3 的 helper，bitmap on
        let dir = FSDirectory::open(&root).unwrap();
        let mut s = Searcher::open(&dir).unwrap();
        let hot_warm = Query::phrase("message", &["hot", "warm"]);
        let battery: Vec<Query> = vec![
            // 跨字段 AND（不可拍平 → 通用路径 fold）
            Query::bool(vec![
                (Occur::Must, Query::term("level", "INFO")),
                (Occur::Must, Query::term("tid", "tid-7")),
            ]),
            // MUST + MUST_NOT
            Query::bool(vec![
                (Occur::Must, Query::term("message", "hot")),
                (Occur::MustNot, Query::term("message", "x")),
            ]),
            // 混合 occur（SHOULD + MUST_NOT）
            Query::bool(vec![
                (Occur::Should, Query::term("message", "warm")),
                (Occur::MustNot, Query::term("tid", "tid-7")),
            ]),
            // 嵌套（外层 MUST + 内层 SHOULD → 不可拍平）
            Query::bool(vec![
                (Occur::Must, Query::term("level", "INFO")),
                (Occur::Must, Query::bool(vec![
                    (Occur::Should, Query::term("message", "hot")),
                    (Occur::Should, Query::term("tid", "tid-8")),
                ])),
            ]),
            // phrase 叶子 + MUST_NOT
            Query::bool(vec![
                (Occur::Must, hot_warm.clone()),
                (Occur::MustNot, Query::term("tid", "tid-7")),
            ]),
            // 纯 MUST_NOT（既有 maxDoc − prohibited 路径，钉死防回归）
            Query::bool(vec![(Occur::MustNot, Query::term("message", "hot"))]),
            // Terms 叶子
            Query::bool(vec![
                (Occur::Must, Query::terms("message", &["hot", "x", "nosuch"])),
                (Occur::MustNot, Query::term("tid", "tid-7")),
            ]),
        ];
        for q in &battery {
            assert_eq!(
                s.count(q).unwrap(),
                drive_count_reference(&dir, q),
                "fold == drive {q:?}"
            );
        }
        // bitmap off 索引同 battery（postings 物化叶子）
        let root_off = temp_dir("foldoff");
        write_phrase_bitmap_corpus(&root_off, false);
        let dir_off = FSDirectory::open(&root_off).unwrap();
        let mut s_off = Searcher::open(&dir_off).unwrap();
        for q in &battery {
            assert_eq!(
                s_off.count(q).unwrap(),
                drive_count_reference(&dir_off, q),
                "fold(off) == drive {q:?}"
            );
        }
        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&root_off).unwrap();
    }

    /// M7 §3.2 成本护栏：budget=0 必 OverBudget；budget=u64::MAX 必 Hits。
    #[test]
    fn fold_cost_guard_triggers() {
        let root = temp_dir("foldguard");
        write_bitmap_corpus(&root, true);
        let dir = FSDirectory::open(&root).unwrap();
        let mut reader = Reader::open(&dir).unwrap();
        let (_b, seg) = reader.leaves().next().unwrap();
        let clauses = vec![
            (Occur::Must, Query::term("message", "hot")),
            (Occur::Must, Query::term("message", "t3")),
        ];
        let mut cost = 0u64;
        let out = query::materialize_bool_bitmap(seg, &clauses, 0, &mut cost).unwrap();
        assert!(matches!(out, query::MatOutcome::OverBudget));
        let mut cost = 0u64;
        let out = query::materialize_bool_bitmap(seg, &clauses, u64::MAX, &mut cost).unwrap();
        match out {
            query::MatOutcome::Hits(bm) => assert_eq!(bm.cardinality(), 714), // hot∧t3 = i%7==3
            query::MatOutcome::OverBudget => panic!("u64::MAX budget must not be over"),
        }
        drop(reader);
        fs::remove_dir_all(&root).unwrap();
    }
```

注：`hot∧t3`：write_bitmap_corpus 里每 doc message = `hot t{i%7}`，t3 命中 i%7==3 → 5000/7 向下取整后逐个算：i∈[0,5000) 且 i%7==3 → 714 个（3,10,...,4998）。若实测不符以 `drive_count_reference` 为准修正字面量。

Run: `RUST_MIN_STACK=4M cargo test -p rustlucene-core --lib fold 2>&1 | tail -5`
Expected: FAIL（`materialize_bool_bitmap`/`MatOutcome` 未定义，编译错误）

- [ ] **Step 2: 实现 MatOutcome + 叶子物化 helper**

`crates/core/src/search/query.rs`（`bool_segment_count` 之前）加：

```rust
/// M7 §3.2 fold 成本护栏：估计物化成本（各叶子 Σdf；Phrase 叶子 = 各
/// term df 和；PointRange 叶子 = maxDoc）> FOLD_COST_FACTOR × maxDoc 时
/// 回落 drive_count（防病态形状回退）。bench 校准后可调。
const FOLD_COST_FACTOR: u64 = 4;

/// T-B 递归物化结果（spec §3.1）。
pub(crate) enum MatOutcome {
    /// 命中集（可为空 bitmap：未知字段 / 缺子句 / 零命中的统一形状）
    Hits(MaterializedBitmap),
    /// 估计物化成本超预算——调用方回落 drive_count
    OverBudget,
}

/// 单 term 叶子物化：有内联 bitmap → 容器级拷贝；无 → postings 全量
/// 扫描（O(df)）。cost 累加 df，超 budget → OverBudget。
fn term_entry_bitmap(
    seg: &SegmentReader,
    entry: &TermEntry,
    has_freqs: bool,
    budget: u64,
    cost: &mut u64,
) -> io::Result<MatOutcome> {
    *cost += entry.doc_freq as u64;
    if *cost > budget {
        return Ok(MatOutcome::OverBudget);
    }
    if let Some(f) = seg.open_term_bitmap(entry)? {
        return Ok(MatOutcome::Hits(f.to_materialized()));
    }
    let mut docs = Vec::with_capacity(entry.doc_freq as usize);
    multi_term::for_each_doc(seg, entry, has_freqs, &mut |d| docs.push(d))?;
    Ok(MatOutcome::Hits(MaterializedBitmap::of(&docs)))
}
```

（`TermEntry` 已在 query.rs 的 use 列表中——确认顶部 `use codec_lucene9::terms_read::TermEntry;`，若无则加。）

- [ ] **Step 3: 实现递归 materialize_query_bitmap / materialize_bool_bitmap**

```rust
/// 递归把任意查询物化为段内 doc bitmap（M7 §3.1，**仅服务 count**：无
/// 提前终止，物化不亏——这是与迭代路径的本质区别）。成本经共享的
/// `cost` 累加器记账，任一叶子超 budget 全树 OverBudget。
fn materialize_query_bitmap(
    seg: &mut SegmentReader,
    query: &Query,
    budget: u64,
    cost: &mut u64,
) -> io::Result<MatOutcome> {
    match query {
        Query::MatchAll => Ok(MatOutcome::Hits(MaterializedBitmap::full(seg.max_doc() as u32))),
        Query::Term { field, term } => match seg.seek_term(field, term)? {
            None => Ok(MatOutcome::Hits(MaterializedBitmap::of(&[]))),
            Some((has_freqs, entry)) => {
                term_entry_bitmap(seg, &entry, has_freqs, budget, cost)
            }
        },
        Query::And { field, terms } | Query::Or { field, terms } => {
            let is_and = matches!(query, Query::And { .. });
            let Some((has_freqs, entries)) =
                roaring_exec::collect_bool_entries(seg, field, terms, is_and)?
            else {
                return Ok(MatOutcome::Hits(MaterializedBitmap::of(&[])));
            };
            fold_term_entries(seg, &entries, has_freqs, is_and, budget, cost)
        }
        Query::Bool { clauses } => materialize_bool_bitmap(seg, clauses, budget, cost),
        Query::Terms { field, terms } => {
            let Some((has_freqs, collected)) = multi_term::collect_direct(seg, field, terms)?
            else {
                return Ok(MatOutcome::Hits(MaterializedBitmap::of(&[])));
            };
            fold_term_entries(seg, &collected.entries, has_freqs, false, budget, cost)
        }
        Query::Prefix { field, prefix } => {
            let Some((has_freqs, collected)) = multi_term::collect_prefix(seg, field, prefix)?
            else {
                return Ok(MatOutcome::Hits(MaterializedBitmap::of(&[])));
            };
            fold_term_entries(seg, &collected.entries, has_freqs, false, budget, cost)
        }
        Query::Wildcard { field, pattern } => {
            let pat = multi_term::WildcardPattern::parse(pattern);
            let Some((has_freqs, collected)) = multi_term::collect_wildcard(seg, field, &pat)?
            else {
                return Ok(MatOutcome::Hits(MaterializedBitmap::of(&[])));
            };
            fold_term_entries(seg, &collected.entries, has_freqs, false, budget, cost)
        }
        Query::PointRange { field, low, high } => {
            *cost += seg.max_doc() as u64; // BKD 无法预估命中，保守计
            if *cost > budget {
                return Ok(MatOutcome::OverBudget);
            }
            let bm = point_range_bitmap(seg, field, *low, *high)?;
            Ok(MatOutcome::Hits(bm.unwrap_or_else(|| MaterializedBitmap::of(&[]))))
        }
        Query::Phrase { field, terms } => {
            // 成本近似 = 各 term df 和（doc 合取扫描量）；缺 term → 空
            for t in terms {
                match seg.seek_term(field, t)? {
                    None => return Ok(MatOutcome::Hits(MaterializedBitmap::of(&[]))),
                    Some((_, entry)) => {
                        *cost += entry.doc_freq as u64;
                        if *cost > budget {
                            return Ok(MatOutcome::OverBudget);
                        }
                    }
                }
            }
            drive_materialize(seg, query)
        }
    }
}

/// 驱动查询的 segment_iterator 全量收集 docs 物化（Phrase 叶子用；
/// 必须调 matches()——T-A 协议）。
fn drive_materialize(seg: &mut SegmentReader, query: &Query) -> io::Result<MatOutcome> {
    let mut docs = Vec::new();
    if let Some(mut it) = query.segment_iterator(seg, false)? {
        loop {
            let d = it.next_doc()?;
            if d == NO_MORE_DOCS {
                break;
            }
            if !it.matches()? {
                continue;
            }
            docs.push(d as u32);
        }
    }
    Ok(MatOutcome::Hits(MaterializedBitmap::of(&docs)))
}

/// term 集 fold：is_and → 交（零 cardinality 短路），否则 → 并。
fn fold_term_entries(
    seg: &SegmentReader,
    entries: &[(u32, TermEntry)],
    has_freqs: bool,
    is_and: bool,
    budget: u64,
    cost: &mut u64,
) -> io::Result<MatOutcome> {
    let mut acc: Option<MaterializedBitmap> = None;
    for (_, entry) in entries {
        let child = match term_entry_bitmap(seg, entry, has_freqs, budget, cost)? {
            MatOutcome::OverBudget => return Ok(MatOutcome::OverBudget),
            MatOutcome::Hits(bm) => bm,
        };
        acc = Some(match (acc, is_and) {
            (None, _) => child,
            (Some(a), true) => a.and(&child),
            (Some(a), false) => a.or(&child),
        });
        if is_and && acc.as_ref().unwrap().cardinality() == 0 {
            return Ok(MatOutcome::Hits(MaterializedBitmap::of(&[]))); // 交集已空，短路
        }
    }
    Ok(MatOutcome::Hits(acc.unwrap_or_else(|| MaterializedBitmap::of(&[]))))
}

/// Bool 子句 fold（M7 §3.1）：正集三态与迭代语义逐条对应——MUST 交 /
/// 纯 SHOULD 并 / 纯 MUST_NOT 的 MatchAll；排除集先并后 andnot。
/// 任一子树 OverBudget → 全树 OverBudget。
pub(crate) fn materialize_bool_bitmap(
    seg: &mut SegmentReader,
    clauses: &[(Occur, Query)],
    budget: u64,
    cost: &mut u64,
) -> io::Result<MatOutcome> {
    let mut musts: Vec<&Query> = Vec::new();
    let mut shoulds: Vec<&Query> = Vec::new();
    let mut nots: Vec<&Query> = Vec::new();
    for (occur, q) in clauses {
        match occur {
            Occur::Must => musts.push(q),
            Occur::Should => shoulds.push(q),
            Occur::MustNot => nots.push(q),
        }
    }
    let mut fold_group = |group: &[&Query], is_and: bool,
                          cost: &mut u64|
     -> io::Result<MatOutcome> {
        let mut acc: Option<MaterializedBitmap> = None;
        for q in *group {
            let child = match materialize_query_bitmap(seg, q, budget, cost)? {
                MatOutcome::OverBudget => return Ok(MatOutcome::OverBudget),
                MatOutcome::Hits(bm) => bm,
            };
            acc = Some(match (acc, is_and) {
                (None, _) => child,
                (Some(a), true) => a.and(&child),
                (Some(a), false) => a.or(&child),
            });
            if is_and && acc.as_ref().unwrap().cardinality() == 0 {
                return Ok(MatOutcome::Hits(MaterializedBitmap::of(&[])));
            }
        }
        Ok(MatOutcome::Hits(acc.unwrap_or_else(|| MaterializedBitmap::of(&[]))))
    };
    let mut positive = if !musts.is_empty() {
        match fold_group(&musts, true, cost)? {
            MatOutcome::OverBudget => return Ok(MatOutcome::OverBudget),
            MatOutcome::Hits(bm) => bm,
        }
    } else if !shoulds.is_empty() {
        match fold_group(&shoulds, false, cost)? {
            MatOutcome::OverBudget => return Ok(MatOutcome::OverBudget),
            MatOutcome::Hits(bm) => bm,
        }
    } else if !nots.is_empty() {
        MaterializedBitmap::full(seg.max_doc() as u32)
    } else {
        return Ok(MatOutcome::Hits(MaterializedBitmap::of(&[])));
    }
    if !nots.is_empty() {
        let prohibited = match fold_group(&nots, false, cost)? {
            MatOutcome::OverBudget => return Ok(MatOutcome::OverBudget),
            MatOutcome::Hits(bm) => bm,
        };
        positive = positive.andnot(&prohibited);
    }
    Ok(MatOutcome::Hits(positive))
}
```

注意：闭包 `fold_group` 捕获 `seg`（`&mut SegmentReader`）与 `budget`——若借用检查报闭包内 `seg` 重复可变借用，把 `fold_group` 改为普通 fn（参数加 `seg: &mut SegmentReader, budget: u64`），调用点相应传参。二选一以实现时编译为准，语义不变。

- [ ] **Step 4: bool_segment_count 插入 fold**

`bool_segment_count`（query.rs:514-540）在纯 MUST_NOT 分支之后、`drive_count` 之前插入：

```rust
    // T-B（M7 §3）：通用形状 count 的 bitmap fold——count-only 无提前
    // 终止，全量物化 + roaring fold 稳赢逐 doc 对齐；超预算回落迭代。
    if !clauses.is_empty() {
        let budget = FOLD_COST_FACTOR * seg.max_doc() as u64;
        let mut cost = 0u64;
        match materialize_bool_bitmap(seg, clauses, budget, &mut cost)? {
            MatOutcome::Hits(bm) => return Ok(bm.cardinality()),
            MatOutcome::OverBudget => {} // 回落 drive_count
        }
    }
    // 通用：组合迭代器逐 doc 计数。
    drive_count(bool_segment_iterator(seg, clauses, false)?)
```

- [ ] **Step 5: 测试 + 全量**

Run: `RUST_MIN_STACK=4M cargo test -p rustlucene-core --lib 2>&1 | tail -3`
Expected: `test result: ok. 81 passed`（79 + 2 新增，全绿）

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/search/query.rs crates/core/src/search/mod.rs
git commit -m "feat: M7 T-B2 通用 Bool count bitmap fold——递归物化 + and/or/andnot + 4×maxDoc 成本护栏回落（count 与逐 doc 迭代等价钉死）"
```

---

### Task 6: Top-N 提前终止（fast_segment_count 统一入口 + Searcher::count 重构 + top_docs 重写）

**Files:**
- Modify: `crates/core/src/search/query.rs`（新增 fast_segment_count / bool_segment_fast_count；bool_segment_count 重构为薄壳）
- Modify: `crates/core/src/search/searcher.rs:56-151`（count 重构 + top_docs 重写）
- Test: `crates/core/src/search/mod.rs`（新增 `topn_early_termination_equivalence`）

**Interfaces:**
- Consumes: Task 5 的 `materialize_bool_bitmap`/`MatOutcome`/`FOLD_COST_FACTOR`；既有 `roaring_exec::{collect_bool_entries, count}`、`Query::bitset_count`、`point_range_bitmap`、`bool_segment_count`。
- Produces: `pub(crate) fn fast_segment_count(seg: &mut SegmentReader, query: &Query) -> io::Result<Option<u64>>`——Some = 段级快路径 count；None = 无快路径（调用方迭代）。`Searcher::top_docs` 新驱动（语义不变量：`(total, docs)` 与旧实现逐字节一致）。

- [ ] **Step 1: 写等价钉死测试**

`crates/core/src/search/mod.rs` tests 模块加：

```rust
    /// M7 T-D：top-N 新旧行为钉死——(total, docs) 与"全程迭代 + 前 N"
    /// 参照逐字节一致；N ∈ {0,1,7,100,6000}，含跨段与无快路径形状。
    fn reference_top_docs(dir: &FSDirectory, q: &Query, n: usize) -> (u64, Vec<i32>) {
        use codec_lucene9::postings_read::NO_MORE_DOCS;
        let mut reader = Reader::open(dir).unwrap();
        let mut total = 0u64;
        let mut docs = Vec::new();
        for (base, seg) in reader.leaves() {
            if let Some(mut it) = q.segment_iterator(seg, false).unwrap() {
                loop {
                    let d = it.next_doc().unwrap();
                    if d == NO_MORE_DOCS {
                        break;
                    }
                    if !it.matches().unwrap() {
                        continue;
                    }
                    total += 1;
                    if docs.len() < n {
                        docs.push(base + d);
                    }
                }
            }
        }
        (total, docs)
    }

    #[test]
    fn topn_early_termination_equivalence() {
        let root = temp_dir("topn");
        write_phrase_bitmap_corpus(&root, true);
        let dir = FSDirectory::open(&root).unwrap();
        let mut s = Searcher::open(&dir).unwrap();
        let battery: Vec<Query> = vec![
            Query::MatchAll,
            Query::term("message", "hot"),            // doc_freq 直读快路径
            Query::term("message", "nosuch"),         // 空
            Query::phrase("message", &["hot", "warm"]), // 无快路径（Phrase）→ 全程迭代
            Query::terms("message", &["hot", "x"]),   // ≤16 OR 路径
            Query::prefix("message", "ho"),           // bitset popcount
            Query::bool(vec![                          // fold 快路径
                (Occur::Must, Query::term("level", "INFO")),
                (Occur::MustNot, Query::term("tid", "tid-7")),
            ]),
            Query::bool(vec![(Occur::MustNot, Query::term("message", "x"))]), // maxDoc−prohibited
            Query::point_range("nope", 0, 1),         // 未知 point 字段 → 0
        ];
        for q in &battery {
            for n in [0usize, 1, 7, 100, 6000] {
                assert_eq!(
                    s.top_docs(q, n).unwrap(),
                    reference_top_docs(&dir, q, n),
                    "top_docs({n}) {q:?}"
                );
            }
        }
        fs::remove_dir_all(&root).unwrap();
        // 跨段边界：bitmap tier 语料（3 段）
        let root_ms = temp_dir("topnms");
        write_tier_corpus(&root_ms, true, 3);
        let dir_ms = FSDirectory::open(&root_ms).unwrap();
        let mut s_ms = Searcher::open(&dir_ms).unwrap();
        let q = Query::or("message", &["alpha", "beta"]); // 以 write_tier_corpus 实际 term 为准
        for n in [0usize, 1, 7, 100, 100_000] {
            assert_eq!(
                s_ms.top_docs(&q, n).unwrap(),
                reference_top_docs(&dir_ms, &q, n),
                "multi-segment top_docs({n})"
            );
        }
        fs::remove_dir_all(&root_ms).unwrap();
    }
```

注：`write_tier_corpus` 的 term 名以其实现为准（mod.rs:817）——实现者读该 helper 后修正查询字面量；若该 helper 语料不含 phrase 字段则跨段部分只用 keyword/term 查询。

Run: `RUST_MIN_STACK=4M cargo test -p rustlucene-core --lib topn_early 2>&1 | tail -3`
Expected: PASS（旧实现即与参照一致——钉死测试，重写后防回归）

- [ ] **Step 2: bool_segment_fast_count 抽出 + bool_segment_count 薄壳化**

`crates/core/src/search/query.rs`，把 `bool_segment_count`（:514-540）重构为：

```rust
/// M7 §5.1 Bool 段级 count 快路径：Some = 快路径结果（拍平 roaring /
/// 纯 MUST_NOT / T-B fold）；None = 无快路径（调用方迭代）。
pub(crate) fn bool_segment_fast_count(
    seg: &mut SegmentReader,
    clauses: &[(Occur, Query)],
) -> io::Result<Option<u64>> {
    // 拍平快路径（§2.4 同形状）：roaring count
    let refs: Vec<(Occur, &Query)> = clauses.iter().map(|(o, q)| (*o, q)).collect();
    if let Some((is_and, field, terms)) = flatten_bool(&refs) {
        if terms.len() >= 2 {
            let Some((has_freqs, entries)) =
                roaring_exec::collect_bool_entries(seg, field, &terms, is_and)?
            else {
                return Ok(Some(0)); // 未知字段 / AND 缺子句 / OR 全缺 → 段内空
            };
            if let Some(c) = roaring_exec::count(seg, &entries, has_freqs, is_and)? {
                return Ok(Some(c));
            }
        }
    }
    // 纯 MUST_NOT（§2.2/§2.5）：MatchAll 排除，count = maxDoc − prohibited。
    if !clauses.is_empty() && clauses.iter().all(|(o, _)| *o == Occur::MustNot) {
        let prohibited = prohibited_count(seg, clauses)?;
        return Ok(Some(seg.max_doc() as u64 - prohibited));
    }
    // T-B（M7 §3）：通用形状 bitmap fold；超预算 None 回落迭代。
    if !clauses.is_empty() {
        let budget = FOLD_COST_FACTOR * seg.max_doc() as u64;
        let mut cost = 0u64;
        match materialize_bool_bitmap(seg, clauses, budget, &mut cost)? {
            MatOutcome::Hits(bm) => return Ok(Some(bm.cardinality())),
            MatOutcome::OverBudget => {}
        }
    }
    Ok(None)
}

/// spec §2.5 Bool per-segment count：快路径（拍平 roaring / 纯 MUST_NOT /
/// T-B fold）优先，None 回落组合迭代器逐 doc 计数。
pub(crate) fn bool_segment_count(
    seg: &mut SegmentReader,
    clauses: &[(Occur, Query)],
) -> io::Result<u64> {
    if let Some(c) = bool_segment_fast_count(seg, clauses)? {
        return Ok(c);
    }
    drive_count(bool_segment_iterator(seg, clauses, false)?)
}
```

- [ ] **Step 3: fast_segment_count 统一入口**

`crates/core/src/search/query.rs` 加：

```rust
/// M7 §5.1 段级 count 快路径统一入口：Some(c) = 快路径结果；None = 无
/// 快路径（调用方迭代计数）。归并既有全部捷径（Term doc_freq 直读 /
/// PointRange bitmap cardinality / multi-term bitset popcount / And-Or
/// roaring count / Bool 快路径族）。Searcher::count 与 top_docs 共用。
pub(crate) fn fast_segment_count(seg: &mut SegmentReader, query: &Query) -> io::Result<Option<u64>> {
    match query {
        Query::MatchAll => Ok(Some(seg.max_doc() as u64)),
        Query::Term { field, term } => Ok(Some(match seg.seek_term(field, term)? {
            Some((_, entry)) => entry.doc_freq as u64,
            None => 0,
        })),
        Query::PointRange { field, low, high } => {
            let bm = point_range_bitmap(seg, field, *low, *high)?;
            Ok(Some(bm.map_or(0, |b| b.cardinality())))
        }
        q if q.is_multi_term() => q.bitset_count(seg),
        Query::And { field, terms } | Query::Or { field, terms } if terms.len() >= 2 => {
            let is_and = matches!(query, Query::And { .. });
            let Some((has_freqs, entries)) =
                roaring_exec::collect_bool_entries(seg, field, terms, is_and)?
            else {
                return Ok(Some(0));
            };
            roaring_exec::count(seg, &entries, has_freqs, is_and)
        }
        Query::Bool { clauses } => bool_segment_fast_count(seg, clauses),
        _ => Ok(None), // Phrase 等：无快路径
    }
}
```

注意：`q if q.is_multi_term()` 分支放 `And|Or` 之前（Terms/Prefix/Wildcard 不是 And/Or 变体，顺序实际无碍，但保持此序防未来变体冲突）。

- [ ] **Step 4: Searcher::count 重构 + top_docs 重写**

`crates/core/src/search/searcher.rs` 的 `count`（:63-144）整体替换为：

```rust
    /// 逐段 count：段级快路径（fast_segment_count，M7 §5.1）优先，
    /// None 回落迭代计数。各形状语义与重构前逐条一致。
    pub fn count(&mut self, query: &Query) -> io::Result<u64> {
        let mut total = 0u64;
        for (_doc_base, seg) in self.reader.leaves() {
            if let Some(c) = query::fast_segment_count(seg, query)? {
                total += c;
                continue;
            }
            if let Some(mut iter) = query.segment_iterator(seg, false)? {
                loop {
                    if iter.next_doc()? == NO_MORE_DOCS {
                        break;
                    }
                    if !iter.matches()? {
                        continue;
                    }
                    total += 1;
                }
            }
        }
        Ok(total)
    }
```

（query.rs 的 `bool_segment_count` 改为 `pub(crate)` 后 searcher 不再直接引用它——把 searcher.rs 顶部 `use super::query::{self, bool_segment_count, Query};` 改为 `use super::query::{self, Query};`；`use super::roaring_exec;` 同样不再被 searcher 直接引用，一并删除，避免 unused-import 警告。）

`top_docs`（:146-151）替换为：

```rust
    /// (total hits, first `n` docIDs ascending) — Sort.INDEXORDER topN。
    /// M7 §5.2：count/topN 分离——段级 count 有快路径时 total 直读
    /// （µs 级），迭代只到收满 n 个命中即停；无快路径的形状回落全程
    /// 迭代（与旧实现一致）。(total, docs) 与旧实现逐字节一致。
    pub fn top_docs(&mut self, query: &Query, n: usize) -> io::Result<(u64, Vec<i32>)> {
        let mut total = 0u64;
        let mut docs: Vec<i32> = Vec::with_capacity(n.min(1024));
        for (doc_base, seg) in self.reader.leaves() {
            let fast = query::fast_segment_count(seg, query)?;
            if let Some(c) = fast {
                total += c;
            }
            // 段按 docBase 升序（INDEXORDER）：收满 n 且 count 已直读 →
            // 后续段只取 count 不再迭代（全局短路）。
            if docs.len() >= n && fast.is_some() {
                continue;
            }
            let Some(mut iter) = query.segment_iterator(seg, false)? else {
                continue; // 段内空（fast=Some(0) 或 None 时均无命中可计）
            };
            loop {
                if docs.len() >= n && fast.is_some() {
                    break; // 段内提前终止
                }
                let doc = iter.next_doc()?;
                if doc == NO_MORE_DOCS {
                    break;
                }
                if !iter.matches()? {
                    continue;
                }
                if fast.is_none() {
                    total += 1;
                }
                if docs.len() < n {
                    docs.push(doc_base + doc);
                }
            }
        }
        Ok((total, docs))
    }
```

- [ ] **Step 5: 测试 + 全量（两个 crate）**

Run: `RUST_MIN_STACK=4M cargo test -p rustlucene-core --lib 2>&1 | tail -3`
Expected: `test result: ok. 82 passed`（81 + 1 新增，全绿）

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/search/query.rs crates/core/src/search/searcher.rs crates/core/src/search/mod.rs
git commit -m "feat: M7 T-D Top-N 提前终止——fast_segment_count 统一 count 快路径入口，top_docs count/topN 分离 + 段内/全局短路（结果与旧实现逐字节一致钉死）"
```

---

### Task 7: 多子句 OR 堆化——micro bench 门槛（≥20% 才落地）

**Files:**
- Test: `crates/core/src/search/mod.rs`（新增 `#[ignore]` bench `disj_over_heap_micro`）
- Modify（仅门槛通过时）: `crates/core/src/search/doc_iter.rs:1025-1088`（DisjOverDocIter 堆化）

**Interfaces:**
- Consumes: `Query::bool` SHOULD×k、`write_bitmap_corpus(root, false)`（bitmap off → 无内联 bitmap，tier 3 / DisjOver 路径）。
- Produces: bench 数据（commit message 记录）；门槛通过 → `DisjOverDocIter` 堆实现（索引堆，不移动 18.7KB 的 SegmentDocIter）。

- [ ] **Step 1: 写堆原型 + micro bench（线性基准 vs 堆对比）**

先在 `crates/core/src/search/doc_iter.rs` 末尾加堆原型（`pub(crate)`，本任务不动生产路径；原型只为 bench，子句恒单阶段故不吸收 matches()——生产化时见 Step 3）：

```rust
/// T-C bench 原型（M7 §4）：DisjOver 的索引堆版本——堆内只放 sub 下标，
/// 18.7KB 的 SegmentDocIter 不挪动。堆序 = sub[i].doc_id() 小顶。
pub(crate) struct DisjOverHeapDocIter {
    sub: Vec<SegmentDocIter>,
    heap: Vec<usize>,
    doc: i32,
}

impl DisjOverHeapDocIter {
    pub(crate) fn new(sub: Vec<SegmentDocIter>) -> io::Result<DisjOverHeapDocIter> {
        debug_assert!(sub.len() >= 2);
        let n = sub.len();
        let mut it = DisjOverHeapDocIter {
            sub,
            heap: (0..n).collect(),
            doc: -1,
        };
        for s in &mut it.sub {
            s.next_doc()?;
        }
        for i in (0..it.heap.len() / 2).rev() {
            it.sift_down(i); // heapify
        }
        Ok(it)
    }
    fn less(&self, a: usize, b: usize) -> bool {
        self.sub[self.heap[a]].doc_id() < self.sub[self.heap[b]].doc_id()
    }
    fn sift_down(&mut self, mut i: usize) {
        loop {
            let (l, r) = (2 * i + 1, 2 * i + 2);
            let mut m = i;
            if l < self.heap.len() && self.less(l, m) {
                m = l;
            }
            if r < self.heap.len() && self.less(r, m) {
                m = r;
            }
            if m == i {
                break;
            }
            self.heap.swap(i, m);
            i = m;
        }
    }
}

impl DocIter for DisjOverHeapDocIter {
    fn doc_id(&self) -> i32 {
        self.doc
    }
    fn next_doc(&mut self) -> io::Result<i32> {
        if self.doc == NO_MORE_DOCS {
            return Ok(NO_MORE_DOCS);
        }
        loop {
            let top = self.heap[0];
            let d = self.sub[top].doc_id();
            if d == NO_MORE_DOCS {
                self.doc = NO_MORE_DOCS;
                return Ok(NO_MORE_DOCS);
            }
            if self.doc < d {
                self.doc = d;
                return Ok(d);
            }
            // 堆顶停在已消费的 doc → 推进并下滤（NO_MORE_DOCS 自然沉底）
            self.sub[top].next_doc()?;
            self.sift_down(0);
        }
    }
}
```

`crates/core/src/search/mod.rs` tests 模块加 `#[ignore]` bench（k ∈ {8, 32, 128} 子句 SHOULD，bitmap off 语料保证 DisjOver/tier-3 路径；每个 arm 独立 Reader——SegmentDocIter 占有 IndexInput 不可复用）：

```rust
    /// M7 T-C：多子句 OR 线性扫描 vs 索引堆 micro bench。
    /// cargo test -p rustlucene-core --lib disj_over_heap_micro -- --ignored --nocapture
    #[test]
    #[ignore = "T-C micro bench"]
    fn disj_over_heap_micro() {
        use codec_lucene9::postings_read::NO_MORE_DOCS;
        use std::time::Instant;
        const DOCS: u32 = 200_000;
        const K_TERMS: u32 = 256; // term 池 t0..t255；df ≈ 3×200k/256 ≈ 2.3k（无 bitmap）
        let root = temp_dir("orheap");
        let mut cfg = IndexWriterConfig::default();
        cfg.bitmap = false;
        let mut w = IndexWriter::create(&root, schema(), cfg).unwrap();
        for i in 0..DOCS {
            let m = format!("t{} t{} t{}", i % K_TERMS, (i / 3) % K_TERMS, (i / 7) % K_TERMS);
            w.add_document(doc("INFO", &format!("tid-{i}"), &m)).unwrap();
        }
        w.commit().unwrap();
        drop(w);
        for k in [8usize, 32, 128] {
            let terms: Vec<String> = (0..k as u32).map(|j| format!("t{j}")).collect();
            // 同一批子句构造两种 DisjOver，驱动到穷尽计时（5 轮取最小）
            let run = |heap: bool| -> u128 {
                let dir = FSDirectory::open(&root).unwrap();
                let mut reader = Reader::open(&dir).unwrap();
                let (_b, seg) = reader.leaves().next().unwrap();
                let mut sub = Vec::new();
                for t in &terms {
                    if let Some(it) =
                        Query::term("message", t).segment_iterator(seg, false).unwrap()
                    {
                        sub.push(it);
                    }
                }
                let start = Instant::now();
                let mut n = 0u64;
                if heap {
                    let mut it = doc_iter::DisjOverHeapDocIter::new(sub).unwrap();
                    loop {
                        if it.next_doc().unwrap() == NO_MORE_DOCS {
                            break;
                        }
                        n += 1;
                    }
                } else {
                    let mut it = doc_iter::DisjOverDocIter::new(sub).unwrap();
                    loop {
                        if it.next_doc().unwrap() == NO_MORE_DOCS {
                            break;
                        }
                        n += 1;
                    }
                }
                assert!(n > 0);
                start.elapsed().as_nanos()
            };
            let (mut lin, mut hp) = (u128::MAX, u128::MAX);
            for _ in 0..5 {
                lin = lin.min(run(false));
                hp = hp.min(run(true));
            }
            println!(
                "k={k}: linear={lin}ns heap={hp}ns heap/linear={:.2}",
                hp as f64 / lin as f64
            );
        }
        fs::remove_dir_all(&root).unwrap();
    }
```

Run: `RUST_MIN_STACK=4M cargo test -p rustlucene-core --lib disj_over_heap_micro -- --ignored --nocapture`
Expected: 打印三行 `k=8/32/128: linear=…ns heap=…ns heap/linear=…`

- [ ] **Step 2: 门槛判定**

- 堆在 k≥32 时收益 **<20%** → 放弃堆化：删除原型，保留 `#[ignore]` bench 与数据，commit message 记录"k=8/32/128 线性 vs 堆 = …，<20% 门槛，放弃（YAGNI）"。**任务到此结束。**
- 收益 **≥20%** → 继续 Step 3。

- [ ] **Step 3:（仅门槛通过）DisjOverDocIter 生产化堆化 + 等价测试**

把生产 `DisjOverDocIter` 替换为堆实现（保留 Task 1 的 matches() 吸收逻辑——堆顶推进后同样对停在 best 的子句 confirmation）。跑全量：

Run: `RUST_MIN_STACK=4M cargo test -p rustlucene-core --lib 2>&1 | tail -3`
Expected: 全绿（既有 bool 电池钉死等价）

- [ ] **Step 4: Commit**

```bash
git add crates/core/src/search/doc_iter.rs crates/core/src/search/mod.rs
git commit -m "feat: M7 T-C 多子句 OR 堆化——k=8/32/128 micro bench 收益 X%（≥20% 门槛通过，线性扫描 → 索引堆）"
# 或放弃时：git commit -m "test: M7 T-C OR 堆化 micro bench——收益 <20% 门槛，放弃堆化（数据记录）"
```

---

### Task 8: 终验——互操作电池 + searchbench 三路 + 账本

**Files:**
- Modify: `.superpowers/sdd/progress.md`（追加 M7 账本行）

**Interfaces:**
- Consumes: Task 1-7 全部。

- [ ] **Step 1: 全量测试（两个 crate）**

Run: `cargo test -p codec-lucene9 --lib 2>&1 | tail -3 && RUST_MIN_STACK=4M cargo test -p rustlucene-core --lib 2>&1 | tail -3`
Expected: 两 crate 全绿

- [ ] **Step 2: 互操作电池**

Run: `make log-test`
Expected: 7 变体全绿（15× INTEROP_OK）

- [ ] **Step 3: searchbench 三路（roaring / pfor / java --no-cache）**

复用 M6 的三路流程（`docs/m3-bench-report.md:201-221` 的 searchbench 调用形状 + M6 账本 `.superpowers/sdd/progress.md` 的 BOOL 行三路 diff 方法；Java 侧 `--no-cache` 既有纪律）。断言：
- 三路 hit-counts diff 为空；
- phrase 嵌套 bool / 通用 bool count 的 rust 侧 qps ≥ 改动前（取 main 基线对比；回归 >15% 须查因）。

- [ ] **Step 4: 账本 + Commit**

```bash
git add -A
git commit -m "test: M7 终验——电池全绿 + searchbench 三路 diff 为空（账本更新）"
```

---

## Self-Review 记录

- **Spec 覆盖**：§2 T-A → Task 1-3；§3 T-B → Task 4-5；§4 T-C → Task 7；§5 T-D → Task 6；§6 验证 → Task 8。工作项 E（DocBlock 批量）spec 明确 deferred，无任务，正确。
- **类型一致性**：`MatOutcome`/`materialize_bool_bitmap`（Task 5 产出）在 Task 6 消费，签名一致；`DocIter::matches`（Task 1 产出）在 Task 2/3/5/6 全部消费；`MaterializedBitmap::{and,or,andnot,full}`（Task 4 产出）在 Task 5 消费，签名一致。
- **顺序依赖**：Task 2 依赖 Task 1（matches 协议）；Task 3 依赖 Task 2；Task 5 依赖 Task 1（drive_materialize 调 matches）与 Task 4；Task 6 依赖 Task 5；Task 7 独立（可在 Task 1 后任意位置）；Task 8 最后。
