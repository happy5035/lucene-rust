# M6 嵌套 Boolean + Point 区间 + forceMerge(1) 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为 rustlucene 补齐三块欠账——真正的嵌套 BooleanQuery（MUST/SHOULD/MUST_NOT）、1D Point 区间查询（BKD 读路径）、forceMerge(1) 格式级段归并。

**Architecture:** 方案一（spec §0）：T-A 嵌套 Bool 用通用组合迭代器 + 同字段纯 Term 子树拍平进现有 roaring 三档；T-B 在 codec 新建 BKD 读路径、命中物化内存 croaring::Bitmap；T-C 复用 T-B 的 BKD 读与新建 DV 读 / .fdx 读做逐格式归并，stored 走块级裸拷贝。T-A 与 T-B 无依赖可并行，T-C 依赖 T-B 排最后。

**Tech Stack:** Rust 1.97.1（codec-lucene9 / rustlucene-core 两 crate）、croaring 2.7.0（已有依赖）、Java 9.12.3 interop 电池（SearchBench 逐条 diff）、`make log-test` 变体。

**Spec:** `docs/superpowers/specs/2026-07-24-rust-m6-bool-point-forcemerge-design.md`（已获用户批准，2026-07-24）

## Global Constraints

- 格式唯一事实来源：`reference/lucene-9.12.3/` 源码，逐条 file:line 注释（项目惯例）。
- codec-lucene9：`#![deny(unsafe_code)]`，模块级 allow 仅 `postings_ll/simd.rs` 与 `roaring/frozen.rs` 两处，**本计划不新增任何 unsafe / allow**。
- rustlucene-core：`#![forbid(unsafe_code)]`；croaring 类型不得穿越进 core 公共 API。
- 不新增第三方依赖；croaring 只经 codec-lucene9 使用。
- 跨任务接口（钉死，三方一致）：
  - `pub enum Occur { Must, Should, MustNot }`；`Query::Bool { clauses: Vec<(Occur, Query)> }`
  - `Query::PointRange { field: String, low: i64, high: i64 }`（双闭区间；`low>high` → `Err(InvalidInput)`）
  - `codec_lucene9::points_read::PointsReader`：`open(dir: &FSDirectory, segment: &str, segment_id: &[u8; 16], field_infos: &FieldInfos) -> io::Result<Option<PointsReader>>`；`intersect(&self, field: &str, low: i64, high: i64, visitor: &mut dyn FnMut(i64, i32)) -> io::Result<()>`（`open` 带 `segment_id` 与 postings_read/terms_read 既有 `check_index_header` 校验惯例一致——T-B 起草评审拍板）
  - `rustlucene_core::merge::force_merge(dir: &FSDirectory, config: &IndexWriterConfig) -> io::Result<()>`
- 每个 commit 前 `cargo fmt`；`cargo test -p codec-lucene9` 与 `cargo test -p rustlucene-core` 全绿才提交。
- commit message 前缀 `feat:` / `test:` / `fix:` / `docs:` / `bench:`（参考 `git log --oneline`）。
- 验收电池：`make log-test` 全绿；bitmap A/B（`RL_BITMAP=0`）diff 为空；Java↔Rust hit counts 逐条 diff 为空。
- 任务顺序：T-A、T-B 可并行；**T-C 必须在 T-B 完成后开工**（消费 PointsReader）。
- 进度记账：每任务完成记 `.superpowers/sdd/progress.md`（gitignored），含 review 结论与 deferred Minor。

---
### Task A: 嵌套 Boolean 查询

**Files:**
- Create: 无（T-A 全部落在既有文件）
- Modify: `crates/core/src/search/query.rs` — `Occur`、`Query::Bool`、`bool()`、`flatten_bool`、`bool_segment_iterator`、`bool_segment_count`；`and/or_segment_iterator` 泛型化
- Modify: `crates/core/src/search/doc_iter.rs` — `ConjOverDocIter`/`DisjOverDocIter`/`ExcludingDocIter` + `SegmentDocIter` 三个新变体
- Modify: `crates/core/src/search/roaring_exec.rs` — `collect_bool_entries` 泛型化（`T: AsRef<[u8]>`）
- Modify: `crates/core/src/search/searcher.rs` — `count()` Bool 分支、`freq_sum()` 拒绝 Bool
- Modify: `crates/core/src/search/mod.rs` — 导出 `Occur`；core 侧全部测试（内置 `#[cfg(test)] mod tests`）
- Modify: `crates/core/src/bin/rustlucene-cli.rs` — `parse_bool_sexpr` 解析器、searchbench BOOL 行接入、searchdump 电池、bin 内解析测试
- Modify: `interop/java/SearchBench.java` — `parseBoolSexpr`、`--load-queries` 重放、`--dump-queries` 生成 BOOL 行
- Modify: `interop/java/VerifySearchIndex.java` — searchdump 对拍电池镜像
- Test: `crates/core/src/search/mod.rs`（core 语义/路径/on-off 等价）、`crates/core/src/bin/rustlucene-cli.rs`（解析测试）

**Interfaces:**
- Consumes:
  - `roaring_exec::collect_bool_entries<T: AsRef<[u8]>>(seg: &mut SegmentReader, field: &str, terms: &[T], is_and: bool) -> io::Result<Option<(bool, Vec<(u32, TermEntry)>)>>`（本任务把它从 `&[Vec<u8>]` 泛型化；`Vec<u8>` 与 `&[u8]` 都实现 `AsRef<[u8]>`，既有调用点零改动、单态化零成本）
  - `roaring_exec::segment_iterator(seg: &SegmentReader, entries: &[(u32, TermEntry)], has_freqs: bool, is_and: bool) -> io::Result<Option<SegmentDocIter>>`（三档入口，`None` = 档 3）
  - `roaring_exec::count(seg: &SegmentReader, entries: &[(u32, TermEntry)], has_freqs: bool, is_and: bool) -> io::Result<Option<u64>>`（count 快路径，`None` = 档 3）
  - `SegmentReader::{seek_term, max_doc, field_has_freqs}`（`crates/core/src/search/segment_reader.rs:36,44,99`）
  - 钉死的跨任务接口（T-B 交付）：`Query::PointRange { field: String, low: i64, high: i64 }` —— 本任务仅在 BOOL 行 RANGE 叶子解析（Step 9）与解析测试里引用，不参与拍平（spec §2.4 拍平仅限同字段 Term 叶子）
- Produces:
  - `pub enum Occur { Must, Should, MustNot }`（`#[derive(Clone, Copy, Debug, PartialEq, Eq)]`，经 `rustlucene_core::search::Occur` 导出）
  - `Query::Bool { clauses: Vec<(Occur, Query)> }`；`pub fn Query::bool(clauses: Vec<(Occur, Query)>) -> Query`
  - `SegmentDocIter::{ConjOver(ConjOverDocIter), DisjOver(DisjOverDocIter), Excluding(ExcludingDocIter)}`；构造签名 `ConjOverDocIter::new(sub: Vec<SegmentDocIter>) -> io::Result<Self>`、`DisjOverDocIter::new(sub: Vec<SegmentDocIter>) -> io::Result<Self>`、`ExcludingDocIter::new(main: SegmentDocIter, prohibited: SegmentDocIter) -> Self`
  - `pub(crate) fn flatten_bool<'q>(clauses: &[(Occur, &'q Query)]) -> Option<(bool, &'q str, Vec<&'q [u8]>)>`（拍平形状判定，借引用零克隆）
  - `pub(crate) fn bool_segment_count(seg: &mut SegmentReader, clauses: &[(Occur, Query)]) -> io::Result<u64>`
  - BOOL 行查询文件格式（下节）+ 两侧解析器：`rustlucene-cli` 的 `fn parse_bool_sexpr(s: &str) -> Result<Query, String>`、`SearchBench.parseBoolSexpr(String) -> Query`（package-static，`VerifySearchIndex` 复用）

#### 查询文件格式：BOOL 行（S 表达式单行）

行格式与既有 `AND\t<bucket>\t<t1>\t<t2>` 同族：`BOOL\t<bucket>\t<sexpr>`，`<sexpr>` 单行、行内无 tab（tab 只作字段分隔）、节点间恰一个空格。Grammar：

```
line       := "BOOL" "\t" bucket "\t" sexpr
sexpr      := "(" node ")"
node       := "TERM" SP field SP term
            | "PREFIX" SP field SP prefix
            | "WILDCARD" SP field SP pattern
            | "PHRASE" SP field SP term SP term          ; 2-term exact phrase（slop=0）
            | "RANGE" SP field SP low SP high            ; i64 双闭区间 → Query::PointRange（T-B）
            | "AND" (SP sexpr)+                          ; 子节点全 Occur::Must
            | "OR" (SP sexpr)+                           ; 子节点全 Occur::Should（Java 侧 msm=1）
            | "NOT" SP sexpr                             ; 单子句 Occur::MustNot
            | "BOOL" (SP occ-clause)+                    ; 混合 occur 容器
occ-clause := "(" ("MUST" | "SHOULD" | "NOT") SP sexpr ")"
field / term / prefix / pattern := 非空白、非括号的 UTF-8 序列
low / high := 十进制 i64；bucket := 聚合标签（low/med/high/bool…），仅用于分组统计
```

每个 TERM 叶子自带 field，因此 BOOL 行天然跨字段（不依赖 searchbench 的 `<field>` 参数——该参数只用于旧行类型）。`AND`/`OR`/`NOT` 是同形 `BOOL` 的语法糖；`BOOL` 节点覆盖 MUST+SHOULD 混合形状（spec §2.6 形状清单要求）。`spec §2.6` 示例里的 `(TERM level INFO WARN)` 多词写法**不支持**——IN 语义写 `(OR (TERM level INFO) (TERM level WARN))`，保持 TERM 叶子 = 字段 + 单词（两侧解析器因此无分歧）。

示例行（log schema；第 3 行含 RANGE，Rust 侧需 T-B 落地后执行）：

```
BOOL	bool	(AND (TERM message connection0) (OR (TERM level INFO) (TERM level WARN)) (NOT (TERM message query23)))
BOOL	bool	(BOOL (MUST (TERM message connection0)) (SHOULD (TERM level INFO)) (NOT (RANGE timestamp 1700000000000 1700000100000)))
BOOL	bool	(OR (TERM level ERROR) (AND (TERM message queue39) (NOT (PHRASE message connection0 query23))))
```

#### 关键设计决定（spec §2 ↔ 代码事实）

1. **SegmentDocIter 栈问题的处置**：`SegmentDocIter` 18.7KB（每个 postings enum 内嵌 8KB `IndexInput` 缓冲，见 `query.rs:199-205` 注释），现有代码的处置是**把 And/Or 分派体 outline 成自由函数**（`and_segment_iterator`/`or_segment_iterator`），避免分派帧在 debug build 溢出 2MiB 测试线程栈。本任务三个新组合器变体全是小 payload（`Vec` 24B / 两个 `Box` 16B，≤40B），enum 尺寸不变（仍由 `Freqs(DocsFreqsEnum)` 决定）；子迭代器住在堆上 Vec 里，递归深度 = 查询嵌套深度（电池最深 3 层）。`bool_segment_iterator` 同样 outline 成自由函数，沿用同一先例。
2. **DisjOver 用线性最小值扫描，不引堆**：spec §2.3 说"参照现有 `DisjunctionDocIter` 的堆实现"——与现状不符，`doc_iter.rs:255-312` 的 `DisjunctionDocIter` 实为线性 min-scan。k = 子句数（通常 <8），DisjOver 逐行镜像该线性实现（对 spec 的一处事实修正，reviewer 注意）。
3. **拍平 = 语义等价改写进 And/Or**：`flatten_bool` 判定"全 MUST（或全 SHOULD）+ 递归展开同形嵌套 Bool 后全部叶子是同字段 Term"（spec §2.4），命中后直接调既有 `and/or_segment_iterator`（含三档 roaring 与档 3 PFOR 回落），零新执行代码。泛型化 `collect_bool_entries` 后，`flatten_bool` 返回借自原查询的 `Vec<&[u8]>`，拍平路径零克隆。
4. **needs_freq 恒 false**（spec §2.3）：Bool 分派忽略入参 `needs_freq`，子句一律按 false 打开；组合器 `freq()` 走 trait 默认 1。`freq_sum()` 在 Searcher 层拒绝 Bool（同 And/Or 先例）。
5. **纯 MUST_NOT 的 Lucene 语义**：`Bool[MustNot x]`（含 x 在段内缺失）= MatchAll 排除 —— 装配判定用"原始 clauses 里是否存在 MUST_NOT"（`has_must_not`），而非"prohibited 迭代器是否为空"，否则 `NOT nosuch` 会错误地返回空而非全量（单测锚点 20/20 钉死）。
6. **T-B 依赖面**：`RANGE` 叶子 → `Query::PointRange` 只在 Step 9 接入（需 T-B 的 variant 已合入；未合入则 Step 9 暂缓，T-A 其余步骤无依赖）。PointRange 子句进 Bool 走通用组合器，**不参与拍平**（spec §2.4）。

**前置**：开工前 `git status` 核实工作区（spec §5.4）；每步 commit 前 `cargo fmt && cargo test -p rustlucene-core` 全绿。

- [ ] **Step 1: `Occur` 三态 + `Query::Bool` 变体 + `bool()` 构造器 + 导出**

先写失败测试（追加到 `crates/core/src/search/mod.rs` 的 `#[cfg(test)] mod tests` 内）：

```rust
    /// M6 §2.1：Occur 三态 + Bool 变体的模型层（构造/匹配/Clone/Eq）。
    #[test]
    fn bool_query_model() {
        let q = Query::bool(vec![
            (Occur::Must, Query::term("message", "w0")),
            (Occur::Should, Query::term("level", "INFO")),
            (Occur::MustNot, Query::MatchAll),
        ]);
        let Query::Bool { clauses } = &q else {
            panic!("expected Bool variant");
        };
        assert_eq!(clauses.len(), 3);
        assert_eq!(clauses[0].0, Occur::Must);
        assert_eq!(clauses[1].0, Occur::Should);
        assert_eq!(clauses[2].0, Occur::MustNot);
        assert_eq!(clauses[2].1, Query::MatchAll);
        let q2 = q.clone();
        assert_eq!(q, q2);
    }
```

跑：`cargo test -p rustlucene-core bool_query_model` —— 预期 FAIL（`Occur`/`Query::bool` 未定义，编译错误）。

实现（`crates/core/src/search/query.rs`）：

文件头注释改为：

```rust
//! Query enum (search spec §3 + M6 §2.1): Term, MatchAll, And, Or, Terms,
//! Prefix, Wildcard, Phrase, Bool. All queries have ConstantScore semantics.
```

`use` 块之后、`pub enum Query` 之前插入：

```rust
/// Boolean clause occur (spec M6 §2.1)：MUST / SHOULD / MUST_NOT；
/// FILTER 不做（ConstantScore 下与 MUST 等价，spec §0 拍板）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Occur {
    Must,
    Should,
    MustNot,
}
```

`Query` 枚举末尾（`Phrase` 变体之后）追加：

```rust
    /// Nested Boolean query (spec M6 §2.1)：子查询可为任意变体（含 Bool
    /// 自身），跨字段。执行语义见 §2.2，拍平规则见 §2.4。
    Bool { clauses: Vec<(Occur, Query)> },
```

`impl Query` 内（`or()` 之后）加构造器：

```rust
    /// Nested Boolean query (spec M6 §2.1)。And/Or 平铺变体保留不删
    /// （bench 与电池在用）；新查询一律用 Bool。
    pub fn bool(clauses: Vec<(Occur, Query)>) -> Query {
        Query::Bool { clauses }
    }
```

`crates/core/src/search/mod.rs` 导出改为：

```rust
pub use query::{Occur, Query};
```

同文件模块文档注释第 2-3 行 `Term/MatchAll/Boolean/multi-term queries` 改为 `Term/MatchAll/Boolean(And/Or/Bool)/multi-term queries`（注释同步，无行为变化）。

`segment_iterator` 的 `match self` 需要临时让 `Bool` 可编译——在 match 追加占位臂（Step 4 替换为真实分派）：

```rust
            Query::Bool { .. } => unimplemented!("Bool segment_iterator lands in Task A Step 4"),
```

跑：`cargo test -p rustlucene-core bool_query_model` —— 预期 PASS。

提交：

```bash
cargo fmt && cargo test -p rustlucene-core
git add crates/core/src/search/query.rs crates/core/src/search/mod.rs
git commit -m "feat: Occur 三态 + Query::Bool 变体与 bool() 构造器（M6 T-A）"
```

- [ ] **Step 2: `flatten_bool` 拍平形状判定（纯函数，spec §2.4）**

先写失败测试（`mod.rs` tests 内追加）：

```rust
    /// M6 §2.4 拍平形状判定：纯 MUST/纯 SHOULD 同字段 Term 子树（递归展开
    /// 同形嵌套 Bool）→ Some；混合 occur / 跨字段 / 非 Term 叶子 / 异形
    /// 嵌套 / 空子树 → None。
    #[test]
    fn flatten_bool_shape_detection() {
        use crate::search::query::flatten_bool;
        let must = |q: Query| (Occur::Must, q);
        let should = |q: Query| (Occur::Should, q);
        let t = |f: &str, s: &str| Query::term(f, s);
        let refs = |v: &Vec<(Occur, Query)>| -> Vec<(Occur, &Query)> {
            v.iter().map(|(o, q)| (*o, q)).collect()
        };
        // 纯 MUST 同字段 → AND 拍平
        let q = vec![must(t("message", "a")), must(t("message", "b"))];
        let (is_and, field, terms) = flatten_bool(&refs(&q)).unwrap();
        assert!(is_and);
        assert_eq!(field, "message");
        assert_eq!(terms, vec![b"a".as_slice(), b"b".as_slice()]);
        // 纯 SHOULD 且嵌套同形 → OR 拍平（递归展开，3 叶子）
        let inner = Query::bool(vec![should(t("message", "b")), should(t("message", "c"))]);
        let q = vec![should(t("message", "a")), should(inner)];
        let (is_and, field, terms) = flatten_bool(&refs(&q)).unwrap();
        assert!(!is_and);
        assert_eq!(field, "message");
        assert_eq!(
            terms,
            vec![b"a".as_slice(), b"b".as_slice(), b"c".as_slice()]
        );
        // 混合 occur → None
        let q = vec![must(t("message", "a")), should(t("message", "b"))];
        assert!(flatten_bool(&refs(&q)).is_none());
        // 跨字段 → None
        let q = vec![must(t("message", "a")), must(t("level", "INFO"))];
        assert!(flatten_bool(&refs(&q)).is_none());
        // 非 Term 叶子（Prefix）→ None
        let q = vec![must(t("message", "a")), must(Query::prefix("message", "a"))];
        assert!(flatten_bool(&refs(&q)).is_none());
        // MUST_NOT → None
        let q = vec![(Occur::MustNot, t("message", "a"))];
        assert!(flatten_bool(&refs(&q)).is_none());
        // AND 内嵌 OR（异形嵌套）→ None
        let inner_or = Query::bool(vec![should(t("message", "b")), should(t("message", "c"))]);
        let q = vec![must(t("message", "a")), must(inner_or)];
        assert!(flatten_bool(&refs(&q)).is_none());
        // 空子树 → None
        let q = vec![must(Query::bool(vec![]))];
        assert!(flatten_bool(&refs(&q)).is_none());
    }
```

跑：`cargo test -p rustlucene-core flatten_bool_shape_detection` —— 预期 FAIL（`flatten_bool` 未定义）。

实现（`query.rs`，放在 `impl Query` 块结束之后、`and_segment_iterator` 之前）：

```rust
/// spec §2.4 拍平形状判定：全部子句同一 occur（全 MUST 或全 SHOULD），
/// 且递归展开**同形**嵌套 Bool 后全部叶子是同字段 Term →
/// Some((is_and, field, terms))（field/terms 借自原查询，零克隆）；
/// 其余形状（混合 occur / 跨字段 / 非 Term 叶子 / 异形嵌套 / 空子树 /
/// 首子句 MUST_NOT）→ None。嵌套 Bool 的 occur 必须与外层一致：
/// `(AND t1 (OR t2 t3))` 是 t1 ∧ (t2∨t3)，不是平 AND，不得拍平。
pub(crate) fn flatten_bool<'q>(
    clauses: &[(Occur, &'q Query)],
) -> Option<(bool, &'q str, Vec<&'q [u8]>)> {
    let is_and = match clauses.first()?.0 {
        Occur::Must => true,
        Occur::Should => false,
        Occur::MustNot => return None,
    };
    let mut field: Option<&'q str> = None;
    let mut terms: Vec<&'q [u8]> = Vec::new();
    for &(occur, q) in clauses {
        let same = matches!(
            (occur, is_and),
            (Occur::Must, true) | (Occur::Should, false)
        );
        if !same {
            return None;
        }
        match q {
            Query::Term { field: f, term } => {
                if field.is_some_and(|hf| hf != f.as_str()) {
                    return None;
                }
                field = Some(f.as_str());
                terms.push(term.as_slice());
            }
            Query::Bool { clauses: sub } => {
                let sub_refs: Vec<(Occur, &Query)> =
                    sub.iter().map(|(o, q)| (*o, q)).collect();
                let (sub_and, sub_field, sub_terms) = flatten_bool(&sub_refs)?;
                if sub_and != is_and {
                    return None;
                }
                if field.is_some_and(|hf| hf != sub_field) {
                    return None;
                }
                field = Some(sub_field);
                terms.extend(sub_terms);
            }
            _ => return None,
        }
    }
    Some((is_and, field?, terms))
}
```

跑：`cargo test -p rustlucene-core flatten_bool_shape_detection` —— 预期 PASS。

提交：

```bash
cargo fmt && cargo test -p rustlucene-core
git add crates/core/src/search/query.rs crates/core/src/search/mod.rs
git commit -m "feat: flatten_bool 拍平形状判定（spec §2.4 纯函数）"
```

- [ ] **Step 3: 泛型化 `collect_bool_entries` 与 `and/or_segment_iterator`（`T: AsRef<[u8]>`，行为不变）**

本步为拍平执行铺路：让三个函数同时接受 `&[Vec<u8>]`（既有 And/Or 调用点）与 `&[&[u8]]`（flatten 借引用结果）。行为不变，用既有测试套件验证。

跑基线：`cargo test -p rustlucene-core` —— 全绿后动手。

改 `crates/core/src/search/roaring_exec.rs:41` 签名与 seek 行：

```rust
/// Collects the (df, entry) pairs of an And/Or's term clauses, df-sorted
/// (conjunction cost order, same as the existing query.rs inline code).
/// Returns (has_freqs, entries); None = empty segment result: unknown
/// field, an absent AND clause, or no OR clause present. M6 T-A：泛型化
/// 到 `AsRef<[u8]>`——`Vec<u8>`（And/Or 平铺变体）与 `&[u8]`（Bool 拍平
/// 的借引用 terms）同入口，单态化零成本。
pub(crate) fn collect_bool_entries<T: AsRef<[u8]>>(
    seg: &mut SegmentReader,
    field: &str,
    terms: &[T],
    is_and: bool,
) -> io::Result<Option<(bool, Vec<(u32, TermEntry)>)>> {
    let Some(has_freqs) = seg.field_has_freqs(field) else {
        return Ok(None);
    };
    let mut entries = Vec::with_capacity(terms.len());
    for t in terms {
        match seg.seek_term(field, t.as_ref())? {
            Some((_, entry)) => entries.push((entry.doc_freq, entry)),
            None => {
                if is_and {
                    return Ok(None); // missing MUST clause: no hits in this segment
                }
            }
        }
    }
    if entries.is_empty() {
        return Ok(None);
    }
    entries.sort_by_key(|(df, _)| *df);
    Ok(Some((has_freqs, entries)))
}
```

改 `crates/core/src/search/query.rs` 的两个 outline 函数（完整替换，含签名、degenerate 臂与注释）：

```rust
/// And/Or arm bodies of `Query::segment_iterator`, outlined into their own
/// frames: `SegmentDocIter` is 18.7KB (every postings enum owns an
/// `IndexInput` with an 8KB inline buffer) and a debug build gives each
/// arm's temporaries disjoint slots in the one dispatcher frame, so inlining
/// the M3 roaring temporaries into both arms pushed the
/// Terms/Prefix/Wildcard → Or → Term rewrite chain over the 2MiB default
/// test-thread stack. The bodies are the spec §5 three-tier rule, per arm.
/// M6 T-A：泛型化 `T: AsRef<[u8]>`——Bool 拍平（spec §2.4）借引用 terms
/// 与 And/Or 的 `Vec<u8>` 共用同一函数体。
fn and_segment_iterator<T: AsRef<[u8]>>(
    seg: &mut SegmentReader,
    field: &str,
    terms: &[T],
    needs_freq: bool,
) -> io::Result<Option<SegmentDocIter>> {
    if terms.len() < 2 {
        return if let Some(t) = terms.first() {
            Query::Term {
                field: field.to_string(),
                term: t.as_ref().to_vec(),
            }
            .segment_iterator(seg, needs_freq)
        } else {
            Ok(None)
        };
    }
    let Some((has_freqs, entries)) = roaring_exec::collect_bool_entries(seg, field, terms, true)?
    else {
        return Ok(None);
    };
    // M3 §5 档 1/2（bitmap 无 freq：needs_freq 永远档 3）
    if !needs_freq {
        if let Some(it) = roaring_exec::segment_iterator(seg, &entries, has_freqs, true)? {
            return Ok(Some(it));
        }
    }
    Ok(Some(SegmentDocIter::And(ConjunctionDocIter::new(
        seg, field, &entries, needs_freq,
    )?)))
}

/// See `and_segment_iterator` — the Or half of the same rule.
fn or_segment_iterator<T: AsRef<[u8]>>(
    seg: &mut SegmentReader,
    field: &str,
    terms: &[T],
    needs_freq: bool,
) -> io::Result<Option<SegmentDocIter>> {
    if terms.len() < 2 {
        return if let Some(t) = terms.first() {
            Query::Term {
                field: field.to_string(),
                term: t.as_ref().to_vec(),
            }
            .segment_iterator(seg, needs_freq)
        } else {
            Ok(None)
        };
    }
    let Some((has_freqs, entries)) = roaring_exec::collect_bool_entries(seg, field, terms, false)?
    else {
        return Ok(None);
    };
    // M3 §5 档 1/2
    if !needs_freq {
        if let Some(it) = roaring_exec::segment_iterator(seg, &entries, has_freqs, false)? {
            return Ok(Some(it));
        }
    }
    Ok(Some(SegmentDocIter::Or(DisjunctionDocIter::new(
        seg, field, &entries, needs_freq,
    )?)))
}
```

既有调用点（`Query::And`/`Query::Or` 臂、`searcher.rs` count 分支）零改动——`&String`→`&str` deref coercion 与 `T = Vec<u8>` 单态化自动成立。

跑：`cargo test -p rustlucene-core` —— 全量 PASS（行为不变的证据 = 既有 and/or/three-tier/skew/fold/multi-segment 测试全绿）。

提交：

```bash
cargo fmt && cargo test -p rustlucene-core
git add crates/core/src/search/query.rs crates/core/src/search/roaring_exec.rs
git commit -m "feat: collect_bool_entries 与 and/or_segment_iterator 泛型化 AsRef<[u8]>（拍平复用铺路，行为不变）"
```

- [ ] **Step 4: 三个通用组合器 + `SegmentDocIter` 新变体 + `bool_segment_iterator` 装配（spec §2.2/§2.3）**

先写失败测试（`mod.rs` tests 内追加；语料形状与 `and_or_single_segment` 相同，命中集注释照抄）：

```rust
    /// M6 Bool 语义语料：与 and_or_single_segment 同一形状——
    /// w0={0,5,8,10,15,16} w1={0,1,6,8,11,16} w2={2,7,12,17}
    /// w3={3,13,18} w4={4,9,14,19}；level INFO={0,4,8,12,16}。
    fn write_bool_corpus(root: &std::path::Path) {
        let mut w = IndexWriter::create(root, schema(), IndexWriterConfig::default()).unwrap();
        for i in 0..20 {
            let level = match i % 4 {
                0 => "INFO",
                1 => "WARN",
                2 => "ERROR",
                _ => "DEBUG",
            };
            let message = if i % 8 == 0 {
                "w0 w1".to_string()
            } else {
                format!("w{}", i % 5)
            };
            w.add_document(doc(level, &format!("tid-{i}"), &message))
                .unwrap();
        }
        w.commit().unwrap();
        drop(w);
    }

    /// spec §2.2 三态执行语义 + 嵌套 + 跨字段 + 段缺失（无 bitmap 语料，
    /// 拍平形走档 3 PFOR，通用形走新组合器——两路径同一测试锚定）。
    #[test]
    fn bool_query_three_state_semantics() {
        let root = temp_dir("bool3state");
        write_bool_corpus(&root);
        let dir = FSDirectory::open(&root).unwrap();
        let mut s = Searcher::open(&dir).unwrap();
        let w = |t: &str| Query::term("message", t);

        // 纯 MUST == And（拍平形）：w0 ∩ w1 = {0,8,16}
        let q = Query::bool(vec![(Occur::Must, w("w0")), (Occur::Must, w("w1"))]);
        assert_eq!(s.count(&q).unwrap(), 3);
        let (total, docs) = s.top_docs(&q, 20).unwrap();
        assert_eq!((total, docs), (3, vec![0, 8, 16]));
        // 纯 SHOULD == Or（拍平形）：|w0 ∪ w1| = 9
        let q = Query::bool(vec![(Occur::Should, w("w0")), (Occur::Should, w("w1"))]);
        assert_eq!(s.count(&q).unwrap(), 9);
        // MUST+SHOULD：SHOULD 被丢弃（spec §2.2）——hits == MUST 单项 w0
        let q = Query::bool(vec![(Occur::Must, w("w0")), (Occur::Should, w("w1"))]);
        let (total, docs) = s.top_docs(&q, 20).unwrap();
        assert_eq!((total, docs), (6, vec![0, 5, 8, 10, 15, 16]));
        // MUST+MUST_NOT：w0 − w1 = {5,10,15}
        let q = Query::bool(vec![(Occur::Must, w("w0")), (Occur::MustNot, w("w1"))]);
        let (total, docs) = s.top_docs(&q, 20).unwrap();
        assert_eq!((total, docs), (3, vec![5, 10, 15]));
        // 纯 MUST_NOT = MatchAll 排除（spec §2.2）：20 − |w1| = 14
        let q = Query::bool(vec![(Occur::MustNot, w("w1"))]);
        let (total, docs) = s.top_docs(&q, 20).unwrap();
        assert_eq!(
            (total, docs),
            (14, vec![2, 3, 4, 5, 7, 9, 10, 12, 13, 14, 15, 17, 18, 19])
        );
        // 纯 MUST_NOT 且 term 缺失 → MatchAll（Lucene 同款语义，钉死）
        let q = Query::bool(vec![(Occur::MustNot, w("nosuch"))]);
        assert_eq!(s.count(&q).unwrap(), 20);
        // 三层嵌套：(w0∪w2) − w1 = {2,5,7,10,12,15,17}
        let q = Query::bool(vec![
            (
                Occur::Must,
                Query::bool(vec![(Occur::Should, w("w0")), (Occur::Should, w("w2"))]),
            ),
            (
                Occur::MustNot,
                Query::bool(vec![(Occur::Must, w("w1"))]),
            ),
        ]);
        let (total, docs) = s.top_docs(&q, 20).unwrap();
        assert_eq!((total, docs), (7, vec![2, 5, 7, 10, 12, 15, 17]));
        // 跨字段：INFO ∩ w0 = {0,8,16}
        let q = Query::bool(vec![
            (Occur::Must, Query::term("level", "INFO")),
            (Occur::Must, w("w0")),
        ]);
        let (total, docs) = s.top_docs(&q, 20).unwrap();
        assert_eq!((total, docs), (3, vec![0, 8, 16]));
        // 空 clauses / SHOULD 全缺 / MUST 缺失 → 0
        assert_eq!(s.count(&Query::bool(vec![])).unwrap(), 0);
        let q = Query::bool(vec![(Occur::Should, w("nosuch"))]);
        assert_eq!(s.count(&q).unwrap(), 0);
        let q = Query::bool(vec![(Occur::Must, w("w0")), (Occur::Must, w("nosuch"))]);
        assert_eq!(s.count(&q).unwrap(), 0);
        fs::remove_dir_all(&root).unwrap();
    }
```

跑：`cargo test -p rustlucene-core bool_query_three_state_semantics` —— 预期 FAIL（`unimplemented!` panic）。

实现分两块。

（a）`crates/core/src/search/doc_iter.rs`：文件头注释末句改为 `M1 adds AND/OR Boolean iterators; M6 §2.3 adds the generic SegmentDocIter combinators (ConjOver/DisjOver/Excluding) for nested Bool.`；在 `// ── SegmentDocIter ──` 分隔注释之前插入三个组合器：

```rust
// ── Generic Boolean combinators over SegmentDocIter (M6 §2.3) ─────────

/// Conjunction over arbitrary per-segment iterators (spec M6 §2.3):
/// the ConjunctionDocIter alignment dance (Lucene ConjunctionDISI
/// protocol) lifted from postings-only PostingsIter to SegmentDocIter
/// children. Children live in a heap Vec — SegmentDocIter is 18.7KB,
/// so no inline child array ever lands in a stack frame.
pub struct ConjOverDocIter {
    sub: Vec<SegmentDocIter>,
    doc: i32,
    lead: usize,
}

impl ConjOverDocIter {
    /// Primes every child (same contract as ConjunctionDocIter::new); any
    /// exhausted child empties the whole conjunction.
    pub fn new(sub: Vec<SegmentDocIter>) -> io::Result<ConjOverDocIter> {
        debug_assert!(sub.len() >= 2);
        let mut it = ConjOverDocIter {
            sub,
            doc: -1,
            lead: 0,
        };
        for s in &mut it.sub {
            if s.next_doc()? == NO_MORE_DOCS {
                it.doc = NO_MORE_DOCS;
                return Ok(it);
            }
        }
        Ok(it)
    }
}

impl DocIter for ConjOverDocIter {
    fn doc_id(&self) -> i32 {
        self.doc
    }
    fn next_doc(&mut self) -> io::Result<i32> {
        if self.doc == NO_MORE_DOCS {
            return Ok(NO_MORE_DOCS);
        }
        if self.doc >= 0 {
            // move every child sitting on the last emitted doc past it
            for i in 0..self.sub.len() {
                if self.sub[i].doc_id() == self.doc
                    && self.sub[i].next_doc()? == NO_MORE_DOCS
                {
                    self.doc = NO_MORE_DOCS;
                    return Ok(NO_MORE_DOCS);
                }
            }
        }
        loop {
            let candidate = self.sub[self.lead].doc_id();
            if candidate == NO_MORE_DOCS {
                self.doc = NO_MORE_DOCS;
                return Ok(NO_MORE_DOCS);
            }
            let mut matched = true;
            for i in 0..self.sub.len() {
                if i == self.lead {
                    continue;
                }
                let d = self.sub[i].advance(candidate)?;
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
    fn advance(&mut self, target: i32) -> io::Result<i32> {
        if self.doc >= target {
            return Ok(self.doc);
        }
        if self.doc == NO_MORE_DOCS {
            return Ok(NO_MORE_DOCS);
        }
        self.sub[self.lead].advance(target)?;
        self.doc = -1;
        self.next_doc()
    }
    // freq: 1（ConstantScore，spec §2.3 Bool 路径恒 needs_freq=false）
}

/// Disjunction over arbitrary per-segment iterators (spec M6 §2.3):
/// DisjunctionDocIter 同款线性最小值 k 路归并（spec 提到"参照堆实现"——
/// 现有 DisjunctionDocIter 实为线性扫描，doc_iter.rs:255-312；k = 子句
/// 数，沿用线性，不引堆）。
pub struct DisjOverDocIter {
    sub: Vec<SegmentDocIter>,
    doc: i32,
}

impl DisjOverDocIter {
    pub fn new(sub: Vec<SegmentDocIter>) -> io::Result<DisjOverDocIter> {
        debug_assert!(sub.len() >= 2);
        let mut it = DisjOverDocIter { sub, doc: -1 };
        for s in &mut it.sub {
            s.next_doc()?;
        }
        Ok(it)
    }
}

impl DocIter for DisjOverDocIter {
    fn doc_id(&self) -> i32 {
        self.doc
    }
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
        let mut best = NO_MORE_DOCS;
        for s in &self.sub {
            let d = s.doc_id();
            if d != NO_MORE_DOCS && d < best {
                best = d;
            }
        }
        self.doc = best;
        Ok(best)
    }
    fn advance(&mut self, target: i32) -> io::Result<i32> {
        if self.doc >= target {
            return Ok(self.doc);
        }
        if self.doc == NO_MORE_DOCS {
            return Ok(NO_MORE_DOCS);
        }
        for s in &mut self.sub {
            if s.doc_id() < target {
                s.advance(target)?;
            }
        }
        let mut best = NO_MORE_DOCS;
        for s in &self.sub {
            let d = s.doc_id();
            if d != NO_MORE_DOCS && d < best {
                best = d;
            }
        }
        self.doc = best;
        Ok(best)
    }
}

/// Exclusion (spec M6 §2.3, Lucene ReqExclScorer two-pointer): main
/// candidates are probe-advanced against prohibited; a collision drops
/// the candidate. 多个 MUST_NOT 由装配方先 DisjOver 合成一个 prohibited。
pub struct ExcludingDocIter {
    main: Box<SegmentDocIter>,
    prohibited: Box<SegmentDocIter>,
    doc: i32,
}

impl ExcludingDocIter {
    pub fn new(main: SegmentDocIter, prohibited: SegmentDocIter) -> ExcludingDocIter {
        ExcludingDocIter {
            main: Box::new(main),
            prohibited: Box::new(prohibited),
            doc: -1,
        }
    }
    /// Emit main's current doc if not prohibited; else advance main past
    /// the collision and retry.
    fn next_non_excluded(&mut self) -> io::Result<i32> {
        loop {
            let d = self.main.doc_id();
            if d == NO_MORE_DOCS {
                self.doc = NO_MORE_DOCS;
                return Ok(NO_MORE_DOCS);
            }
            if self.prohibited.advance(d)? != d {
                self.doc = d;
                return Ok(d);
            }
            if self.main.next_doc()? == NO_MORE_DOCS {
                self.doc = NO_MORE_DOCS;
                return Ok(NO_MORE_DOCS);
            }
        }
    }
}

impl DocIter for ExcludingDocIter {
    fn doc_id(&self) -> i32 {
        self.doc
    }
    fn next_doc(&mut self) -> io::Result<i32> {
        if self.doc == NO_MORE_DOCS {
            return Ok(NO_MORE_DOCS);
        }
        if self.main.next_doc()? == NO_MORE_DOCS {
            self.doc = NO_MORE_DOCS;
            return Ok(NO_MORE_DOCS);
        }
        self.next_non_excluded()
    }
    fn advance(&mut self, target: i32) -> io::Result<i32> {
        if self.doc >= target {
            return Ok(self.doc);
        }
        if self.doc == NO_MORE_DOCS {
            return Ok(NO_MORE_DOCS);
        }
        self.main.advance(target)?;
        self.next_non_excluded()
    }
}
```

`SegmentDocIter` 枚举追加三个变体：

```rust
pub enum SegmentDocIter {
    Docs(DocsEnum),
    Freqs(DocsFreqsEnum),
    All(MatchAllIter),
    And(ConjunctionDocIter),
    Or(DisjunctionDocIter),
    Bitset(BitsetDocIter),
    Phrase(PhraseDocIter),
    Roaring(RoaringDocIter),
    RoaringAnd(RoaringAndDocIter),
    RoaringOr(RoaringOrDocIter),
    // M6 §2.3 通用组合器：全是 Vec/Box 小 payload（≤40B），enum 尺寸
    // 不变（仍由 Freqs 的 18.7KB 决定），栈预算不受新变体影响。
    ConjOver(ConjOverDocIter),
    DisjOver(DisjOverDocIter),
    Excluding(ExcludingDocIter),
}
```

`impl DocIter for SegmentDocIter` 的 `doc_id`/`next_doc`/`advance` 三个 match 各追加三臂（`freq` 不动——catch-all `_ => 1` 已覆盖）：

```rust
            Self::ConjOver(c) => c.doc_id(),
            Self::DisjOver(d) => d.doc_id(),
            Self::Excluding(e) => e.doc_id(),
```
```rust
            Self::ConjOver(c) => c.next_doc(),
            Self::DisjOver(d) => d.next_doc(),
            Self::Excluding(e) => e.next_doc(),
```
```rust
            Self::ConjOver(c) => c.advance(t),
            Self::DisjOver(d) => d.advance(t),
            Self::Excluding(e) => e.advance(t),
```

（b）`crates/core/src/search/query.rs`：`use` 块改为：

```rust
use super::doc_iter::{
    ConjOverDocIter, ConjunctionDocIter, DisjOverDocIter, DisjunctionDocIter,
    ExcludingDocIter, MatchAllIter, PhraseDocIter, RoaringDocIter, SegmentDocIter,
};
```

`segment_iterator` 的占位臂替换为：

```rust
            Query::Bool { clauses } => bool_segment_iterator(seg, clauses, needs_freq),
```

文件末尾（`or_segment_iterator` 之后）追加装配函数：

```rust
/// Bool 分派体（outline 自由函数，与 and/or_segment_iterator 同因：
/// SegmentDocIter 18.7KB，分派帧内联多分支临时量会在 debug build 溢出
/// 2MiB 测试线程栈——见 and_segment_iterator 上方注释）。needs_freq 按
/// spec §2.3 恒 false 处理：freq_sum 已拒绝 Bool，组合语义下 freq 无
/// 定义，子句一律按 no-freq 打开（bitmap/roaring 路径不受限）。
fn bool_segment_iterator(
    seg: &mut SegmentReader,
    clauses: &[(Occur, Query)],
    _needs_freq: bool,
) -> io::Result<Option<SegmentDocIter>> {
    if clauses.is_empty() {
        return Ok(None);
    }
    // 拍平（spec §2.4）：纯 MUST / 纯 SHOULD 同字段 Term 子树 → And/Or
    // 三档（roaring 档 1/2、PFOR 档 3），与平铺变体同一引擎。
    let refs: Vec<(Occur, &Query)> = clauses.iter().map(|(o, q)| (*o, q)).collect();
    if let Some((is_and, field, terms)) = flatten_bool(&refs) {
        return if is_and {
            and_segment_iterator(seg, field, &terms, false)
        } else {
            or_segment_iterator(seg, field, &terms, false)
        };
    }
    // 通用装配（spec §2.2 三态 + §2.3 组合器）。
    let mut musts: Vec<SegmentDocIter> = Vec::new();
    let mut shoulds: Vec<SegmentDocIter> = Vec::new();
    let mut nots: Vec<SegmentDocIter> = Vec::new();
    let mut has_must_not = false;
    for (occur, q) in clauses {
        let it = q.segment_iterator(seg, false)?;
        match (occur, it) {
            (Occur::Must, Some(it)) => musts.push(it),
            // MUST 子句段内缺失 → 全段空（spec §2.2/§2.4 collect 同款语义）
            (Occur::Must, None) => return Ok(None),
            (Occur::Should, it) => {
                if let Some(it) = it {
                    shoulds.push(it);
                }
            }
            (Occur::MustNot, it) => {
                has_must_not = true;
                if let Some(it) = it {
                    nots.push(it);
                }
            }
        }
    }
    // 正集三态：MUST 合取 / 纯 SHOULD 并集 / 纯 MUST_NOT 的 MatchAll。
    // 注意用 has_must_not 而非 nots.is_empty()：NOT 一个段内缺失的 term
    // 语义是 MatchAll − ∅ = MatchAll（Lucene 同款），不是空。
    let positive = if !musts.is_empty() {
        conj_over(musts)?.expect("musts non-empty")
    } else if !shoulds.is_empty() {
        disj_over(shoulds)?.expect("shoulds non-empty")
    } else if has_must_not {
        SegmentDocIter::All(MatchAllIter::new(seg.max_doc()))
    } else {
        return Ok(None); // SHOULD 全缺且无 MUST/MUST_NOT → 段内空
    };
    // 排除集：多个 MUST_NOT 先析取合成一个 prohibited（spec §2.3）。
    Ok(Some(match disj_over(nots)? {
        Some(prohibited) => SegmentDocIter::Excluding(ExcludingDocIter::new(positive, prohibited)),
        None => positive,
    }))
}

/// Vec 装配：≥2 子句包 ConjOver，单子句直接返回（零包装税），空 → None。
fn conj_over(mut its: Vec<SegmentDocIter>) -> io::Result<Option<SegmentDocIter>> {
    match its.len() {
        0 => Ok(None),
        1 => Ok(its.pop()),
        _ => Ok(Some(SegmentDocIter::ConjOver(ConjOverDocIter::new(its)?))),
    }
}

/// Vec 装配：≥2 子句包 DisjOver，单子句直接返回，空 → None。
fn disj_over(mut its: Vec<SegmentDocIter>) -> io::Result<Option<SegmentDocIter>> {
    match its.len() {
        0 => Ok(None),
        1 => Ok(its.pop()),
        _ => Ok(Some(SegmentDocIter::DisjOver(DisjOverDocIter::new(its)?))),
    }
}
```

跑：`cargo test -p rustlucene-core bool_query_three_state_semantics` —— 预期 PASS；再跑 `cargo test -p rustlucene-core` 全量 PASS。

提交：

```bash
cargo fmt && cargo test -p rustlucene-core
git add crates/core/src/search/doc_iter.rs crates/core/src/search/query.rs crates/core/src/search/mod.rs
git commit -m "feat: ConjOver/DisjOver/Excluding 通用组合器 + Bool 三态装配（spec §2.2/§2.3）"
```

- [ ] **Step 5: 混合子句类型 + 拍平 roaring 路径断言 + bitmap on/off 等价（纯测试提交）**

`mod.rs` tests 内追加两个测试：

```rust
    /// spec §2.6 形状覆盖：Phrase/Prefix/Wildcard 子句进 Bool 组合器。
    #[test]
    fn bool_query_mixed_clause_types() {
        // Phrase 子句（positions 语料）：phrase(quick brown)={0,2} − {2} = {0}
        let root = temp_dir("boolphrase");
        write_phrase_corpus(&root);
        let dir = FSDirectory::open(&root).unwrap();
        let mut s = Searcher::open(&dir).unwrap();
        let q = Query::bool(vec![
            (Occur::Must, Query::phrase("message", &["quick", "brown"])),
            (Occur::MustNot, Query::term("tid", "tid-2")),
        ]);
        let (total, docs) = s.top_docs(&q, 10).unwrap();
        assert_eq!((total, docs), (1, vec![0]));
        fs::remove_dir_all(&root).unwrap();

        // Prefix / Wildcard 子句（terms 语料：doc i 带 t(i%20) 与 t((i+7)%20)）
        let root = temp_dir("boolmt");
        write_terms_corpus(&root);
        let dir = FSDirectory::open(&root).unwrap();
        let mut s = Searcher::open(&dir).unwrap();
        // prefix t1 → t10..t19 命中 {3..19, 23..39}（34 doc）；NOT t07
        // （t07={0,7,20,27}，7/27 在并集内）→ 34 − 2 = 32
        let q = Query::bool(vec![
            (Occur::Must, Query::prefix("message", "t1")),
            (Occur::MustNot, Query::term("message", "t07")),
        ]);
        assert_eq!(s.count(&q).unwrap(), 32);
        // wildcard t?7 → t07 ∪ t17（8 doc）；SHOULD t00 被丢弃（spec §2.2）
        let q = Query::bool(vec![
            (Occur::Must, Query::wildcard("message", "t?7")),
            (Occur::Should, Query::term("message", "t00")),
        ]);
        assert_eq!(s.count(&q).unwrap(), 8);
        fs::remove_dir_all(&root).unwrap();
    }

    /// spec §2.4 拍平路径：bitmap 索引上拍平形命中 roaring 三档（变体断言
    /// 钉死），非拍平形走通用组合器；bitmap off/on 全量结果逐位一致
    /// （RL_BITMAP=0 等价物的单测形态——同语料双索引 A/B）。
    #[test]
    fn bool_query_flatten_roaring_paths() {
        let root_off = temp_dir("bflatoff");
        let root_on = temp_dir("bflaton");
        write_tier_corpus(&root_off, false, 1);
        write_tier_corpus(&root_on, true, 1);
        let must = |q: Query| (Occur::Must, q);
        let should = |q: Query| (Occur::Should, q);
        let t = |s: &str| Query::term("message", s);
        // 拍平 AND（嵌套同形）：hot ∧ scorching ∧ warm5
        let flat_and = Query::bool(vec![
            must(t("hot")),
            must(Query::bool(vec![must(t("scorching")), must(t("warm5"))])),
        ]);
        // 拍平 OR：scorching ∨ warm3
        let flat_or = Query::bool(vec![should(t("scorching")), should(t("warm3"))]);
        // 非拍平：AND 内嵌 OR → ConjOver；hits = hot ∧ (scorching∨warm3)
        let nested_or = Query::bool(vec![
            must(t("hot")),
            must(Query::bool(vec![should(t("scorching")), should(t("warm3"))])),
        ]);
        // SHOULD+MUST_NOT → 顶层 Excluding；(warm1∨warm3) − warm5
        let excluding = Query::bool(vec![
            should(t("warm1")),
            should(t("warm3")),
            (Occur::MustNot, t("warm5")),
        ]);
        // 纯 MUST_NOT → Excluding(MatchAll, term)
        let pure_not = Query::bool(vec![(Occur::MustNot, t("warm3"))]);

        // —— 路径断言（bitmap 索引）——
        let dir_on = FSDirectory::open(&root_on).unwrap();
        let mut reader = Reader::open(&dir_on).unwrap();
        let (_base, seg) = reader.leaves().next().unwrap();
        let it = flat_and.segment_iterator(seg, false).unwrap().unwrap();
        assert!(
            matches!(it, SegmentDocIter::RoaringAnd(_)),
            "flat nested AND must take the roaring three-tier path"
        );
        let it = flat_or.segment_iterator(seg, false).unwrap().unwrap();
        assert!(
            matches!(it, SegmentDocIter::RoaringOr(_)),
            "flat OR must take the roaring three-tier path"
        );
        // needs_freq=true 也走 roaring：Bool 路径恒 needs_freq=false
        // （spec §2.3，ConstantScore 化简）
        let it = flat_and.segment_iterator(seg, true).unwrap().unwrap();
        assert!(
            matches!(it, SegmentDocIter::RoaringAnd(_)),
            "Bool ignores needs_freq (freq undefined under combination)"
        );
        let it = nested_or.segment_iterator(seg, false).unwrap().unwrap();
        assert!(
            matches!(it, SegmentDocIter::ConjOver(_)),
            "AND(OR) is not a flat shape: generic combinator"
        );
        let it = excluding.segment_iterator(seg, false).unwrap().unwrap();
        assert!(
            matches!(it, SegmentDocIter::Excluding(_)),
            "SHOULD + MUST_NOT: top-level Excluding"
        );
        let it = pure_not.segment_iterator(seg, false).unwrap().unwrap();
        assert!(
            matches!(it, SegmentDocIter::Excluding(_)),
            "pure MUST_NOT: Excluding over MatchAll"
        );
        drop(reader);

        // —— on/off 全量等价（count + 完整 doc 序列）——
        let mut s_off = Searcher::open(&FSDirectory::open(&root_off).unwrap()).unwrap();
        let mut s_on = Searcher::open(&FSDirectory::open(&root_on).unwrap()).unwrap();
        let battery: Vec<Query> = vec![flat_and, flat_or, nested_or, excluding, pure_not];
        for q in &battery {
            let (a_total, a_docs) = s_off.top_docs(q, 6000).unwrap();
            let (b_total, b_docs) = s_on.top_docs(q, 6000).unwrap();
            assert_eq!((a_total, a_docs), (b_total, b_docs), "top_docs {q:?}");
            assert_eq!(
                s_off.count(q).unwrap(),
                s_on.count(q).unwrap(),
                "count {q:?}"
            );
        }
        // 数值锚点（独立推演，防 on/off 同错）：
        let mut s = Searcher::open(&FSDirectory::open(&root_on).unwrap()).unwrap();
        assert_eq!(s.count(&battery[0]).unwrap(), 643); // scorching ∧ warm5: d≥500 ∧ d%7==5
        assert_eq!(s.count(&battery[1]).unwrap(), 4571); // scorching ∨ warm3
        assert_eq!(s.count(&battery[2]).unwrap(), 4571); // hot ∧ (scorching∨warm3)
        assert_eq!(s.count(&battery[3]).unwrap(), 1429); // (warm1∨warm3) − warm5（互不相交）
        assert_eq!(s.count(&battery[4]).unwrap(), 4286); // 5000 − |warm3|
        fs::remove_dir_all(&root_off).unwrap();
        fs::remove_dir_all(&root_on).unwrap();
    }
```

跑：`cargo test -p rustlucene-core bool_query_` —— 两个新测试 PASS；`cargo test -p rustlucene-core` 全量 PASS。

提交：

```bash
cargo fmt && cargo test -p rustlucene-core
git add crates/core/src/search/mod.rs
git commit -m "test: Bool 混合子句类型 + 拍平 roaring 路径断言 + bitmap on/off 等价锚点"
```

- [ ] **Step 6: `Searcher::count` Bool 分支（拍平快路径 + 纯 MUST_NOT maxDoc 捷径，spec §2.5）**

本步是性能快路径，hit count 数值不可观测地变化（前后一致）——测试为等价性锚定。先写测试（`mod.rs` tests 内追加）：

```rust
    /// spec §2.5：Bool count 与迭代同结构——对每个形状 count == 逐 doc
    /// 迭代总数；纯 MUST_NOT 锚点钉死 maxDoc − prohibited 捷径。
    #[test]
    fn bool_count_matches_iteration() {
        let root = temp_dir("boolcount");
        write_bool_corpus(&root);
        let dir = FSDirectory::open(&root).unwrap();
        let mut s = Searcher::open(&dir).unwrap();
        let w = |t: &str| Query::term("message", t);
        let battery: Vec<Query> = vec![
            Query::bool(vec![]),
            Query::bool(vec![(Occur::Must, w("w0"))]),
            Query::bool(vec![(Occur::Must, w("w0")), (Occur::Must, w("w1"))]),
            Query::bool(vec![(Occur::Should, w("w0")), (Occur::Should, w("w1"))]),
            Query::bool(vec![(Occur::Must, w("w0")), (Occur::Should, w("w1"))]),
            Query::bool(vec![(Occur::Must, w("w0")), (Occur::MustNot, w("w1"))]),
            Query::bool(vec![(Occur::MustNot, w("w1"))]),
            Query::bool(vec![(Occur::MustNot, w("w1")), (Occur::MustNot, w("w2"))]),
            Query::bool(vec![(Occur::MustNot, w("nosuch"))]),
            Query::bool(vec![(Occur::Should, w("w0")), (Occur::MustNot, w("w1"))]),
            Query::bool(vec![
                (
                    Occur::Must,
                    Query::bool(vec![(Occur::Should, w("w0")), (Occur::Should, w("w2"))]),
                ),
                (Occur::MustNot, Query::bool(vec![(Occur::Must, w("w1"))])),
            ]),
        ];
        for q in &battery {
            let (total, _) = s.top_docs(q, 100).unwrap();
            assert_eq!(s.count(q).unwrap(), total, "count == iteration for {q:?}");
        }
        // 纯 MUST_NOT 锚点：|w1|=6 → 14；|w1∪w2|=10（不相交）→ 10
        let q = Query::bool(vec![(Occur::MustNot, w("w1"))]);
        assert_eq!(s.count(&q).unwrap(), 14);
        let q = Query::bool(vec![(Occur::MustNot, w("w1")), (Occur::MustNot, w("w2"))]);
        assert_eq!(s.count(&q).unwrap(), 10);
        fs::remove_dir_all(&root).unwrap();
    }
```

跑：`cargo test -p rustlucene-core bool_count_matches_iteration` —— 现状即 PASS（count 走 CountCollector 驱动），本步落地后重跑确认数值不变。

实现。（a）`query.rs` 的 `use` 块加 `DocIter` 与 `NO_MORE_DOCS`：

```rust
use codec_lucene9::postings_read::NO_MORE_DOCS;

use super::doc_iter::{
    ConjOverDocIter, ConjunctionDocIter, DisjOverDocIter, DisjunctionDocIter, DocIter,
    ExcludingDocIter, MatchAllIter, PhraseDocIter, RoaringDocIter, SegmentDocIter,
};
```

（b）`query.rs` 末尾追加：

```rust
/// spec §2.5 Bool per-segment count：与迭代同一结构——拍平形命中
/// roaring count 快路径（档 1 cardinality 折叠 / 档 2 驱动计数）；纯
/// MUST_NOT 走 maxDoc − prohibited count（避免全量迭代）；其余形状驱动
/// 组合迭代器逐 doc 计数。
pub(crate) fn bool_segment_count(
    seg: &mut SegmentReader,
    clauses: &[(Occur, Query)],
) -> io::Result<u64> {
    // 拍平快路径（§2.4 同形状）：roaring count；档 3 落档（None）与非
    // 拍平形继续向下走通用路径。
    let refs: Vec<(Occur, &Query)> = clauses.iter().map(|(o, q)| (*o, q)).collect();
    if let Some((is_and, field, terms)) = flatten_bool(&refs) {
        if terms.len() >= 2 {
            let Some((has_freqs, entries)) =
                roaring_exec::collect_bool_entries(seg, field, &terms, is_and)?
            else {
                return Ok(0); // 未知字段 / AND 缺子句 / OR 全缺 → 段内空
            };
            if let Some(c) = roaring_exec::count(seg, &entries, has_freqs, is_and)? {
                return Ok(c);
            }
        }
    }
    // 纯 MUST_NOT（§2.2/§2.5）：MatchAll 排除，count = maxDoc − prohibited。
    if !clauses.is_empty() && clauses.iter().all(|(o, _)| *o == Occur::MustNot) {
        let prohibited = prohibited_count(seg, clauses)?;
        return Ok(seg.max_doc() as u64 - prohibited);
    }
    // 通用：组合迭代器逐 doc 计数。
    drive_count(bool_segment_iterator(seg, clauses, false)?)
}

/// 纯 MUST_NOT 的 prohibited 侧 count：子句换 SHOULD 视角取并集——拍平
/// OR 形命中 roaring count 快路径，否则驱动 DisjOver（单子句直接驱动）。
fn prohibited_count(seg: &mut SegmentReader, clauses: &[(Occur, Query)]) -> io::Result<u64> {
    let as_should: Vec<(Occur, &Query)> =
        clauses.iter().map(|(_, q)| (Occur::Should, q)).collect();
    if let Some((_, field, terms)) = flatten_bool(&as_should) {
        if terms.len() >= 2 {
            let Some((has_freqs, entries)) =
                roaring_exec::collect_bool_entries(seg, field, &terms, false)?
            else {
                return Ok(0);
            };
            if let Some(c) = roaring_exec::count(seg, &entries, has_freqs, false)? {
                return Ok(c);
            }
        }
    }
    let mut nots: Vec<SegmentDocIter> = Vec::new();
    for (_, q) in clauses {
        if let Some(it) = q.segment_iterator(seg, false)? {
            nots.push(it);
        }
    }
    drive_count(disj_over(nots)?)
}

/// 驱动迭代器到穷尽计数（None = 段内空）。
fn drive_count(it: Option<SegmentDocIter>) -> io::Result<u64> {
    let mut n = 0u64;
    if let Some(mut it) = it {
        loop {
            if it.next_doc()? == NO_MORE_DOCS {
                break;
            }
            n += 1;
        }
    }
    Ok(n)
}
```

（c）`crates/core/src/search/searcher.rs`：`use super::query::{bool_segment_count, Query};`；`count()` 的 And/Or 分支之后、CountCollector 兜底之前插入：

```rust
        // M6 §2.5: Bool count 与迭代同一结构——拍平形 roaring 快路径、纯
        // MUST_NOT 走 maxDoc − prohibited、其余组合迭代计数（按段独立）。
        if let Query::Bool { clauses } = query {
            let mut total = 0u64;
            for (_doc_base, seg) in self.reader.leaves() {
                total += bool_segment_count(seg, clauses)?;
            }
            return Ok(total);
        }
```

跑：`cargo test -p rustlucene-core` —— 全量 PASS（含 Step 4-6 全部 Bool 锚点）。

提交：

```bash
cargo fmt && cargo test -p rustlucene-core
git add crates/core/src/search/query.rs crates/core/src/search/searcher.rs crates/core/src/search/mod.rs
git commit -m "feat: Searcher::count Bool 分支——拍平 roaring 快路径 + 纯 MUST_NOT maxDoc 捷径（spec §2.5）"
```

- [ ] **Step 7: `freq_sum` 拒绝 Bool（spec §2.3）**

先写失败测试（`mod.rs` tests 内追加）：

```rust
    /// spec §2.3：freq_sum 对 Bool 拒绝（同 And/Or / multi-term 先例）。
    #[test]
    fn freq_sum_rejects_bool_queries() {
        let root = temp_dir("boolfreqsum");
        write_terms_corpus(&root);
        let dir = FSDirectory::open(&root).unwrap();
        let mut s = Searcher::open(&dir).unwrap();
        let q = Query::bool(vec![
            (Occur::Must, Query::term("message", "t00")),
            (Occur::MustNot, Query::term("message", "t07")),
        ]);
        let err = s.freq_sum(&q).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        // Term 不受影响
        assert_eq!(s.freq_sum(&Query::term("message", "t00")).unwrap(), 4);
        fs::remove_dir_all(&root).unwrap();
    }
```

跑：`cargo test -p rustlucene-core freq_sum_rejects_bool_queries` —— 预期 FAIL（`freq_sum` 返回 Ok 而非 Err）。

实现（`searcher.rs` `freq_sum` 的拒绝条件与报错文案）：

```rust
        if query.is_multi_term()
            || matches!(query, Query::And { .. } | Query::Or { .. } | Query::Bool { .. })
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "freq_sum is only defined for Term queries (MatchAll degenerates to \
                 doc count); multi-term and Boolean (And/Or/Bool) queries have no \
                 well-defined freq sum",
            ));
        }
```

同时把 `freq_sum` 的 doc 注释里 "multi-term and Boolean queries have no well-defined freq sum" 一句保留（已覆盖 Bool 语义，无需再改）。

跑：`cargo test -p rustlucene-core freq_sum_rejects` —— 两个拒绝测试 PASS；`cargo test -p rustlucene-core` 全量 PASS。

提交：

```bash
cargo fmt && cargo test -p rustlucene-core
git add crates/core/src/search/searcher.rs crates/core/src/search/mod.rs
git commit -m "feat: freq_sum 拒绝 Bool（ConstantScore 组合下 freq 无定义）"
```

- [ ] **Step 8: rustlucene-cli BOOL 行解析 + searchbench 接入（Rust 侧）**

先写失败测试（`crates/core/src/bin/rustlucene-cli.rs` 文件末尾新增 test 模块）：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// M6 §2.6 BOOL 行 S 表达式解析：叶子/同形节点/嵌套/混合 BOOL/错误。
    #[test]
    fn parse_bool_sexpr_shapes() {
        // TERM 叶子 + AND（全 Must）
        let q = parse_bool_sexpr("(AND (TERM message error) (TERM level INFO))").unwrap();
        let Query::Bool { clauses } = &q else {
            panic!("top must be Bool");
        };
        assert_eq!(clauses.len(), 2);
        assert!(clauses.iter().all(|(o, _)| *o == Occur::Must));
        assert_eq!(clauses[0].1, Query::term("message", "error"));
        assert_eq!(clauses[1].1, Query::term("level", "INFO"));
        // OR（全 Should）+ 三层嵌套 + NOT
        let q = parse_bool_sexpr(
            "(OR (TERM message a) (AND (TERM level INFO) (NOT (TERM source tmp))))",
        )
        .unwrap();
        let Query::Bool { clauses } = &q else {
            panic!("top must be Bool");
        };
        assert_eq!(clauses.len(), 2);
        assert!(clauses.iter().all(|(o, _)| *o == Occur::Should));
        let Query::Bool { clauses: and_clauses } = &clauses[1].1 else {
            panic!("second child must be Bool");
        };
        assert_eq!(and_clauses[1].0, Occur::Must);
        let Query::Bool { clauses: not_clauses } = &and_clauses[1].1 else {
            panic!("NOT child must be Bool");
        };
        assert_eq!(not_clauses.len(), 1);
        assert_eq!(not_clauses[0].0, Occur::MustNot);
        assert_eq!(not_clauses[0].1, Query::term("source", "tmp"));
        // PREFIX / WILDCARD / PHRASE 叶子
        let q = parse_bool_sexpr(
            "(AND (PREFIX message conn) (WILDCARD message c*n) (PHRASE message quick brown))",
        )
        .unwrap();
        let Query::Bool { clauses } = &q else {
            panic!("top must be Bool");
        };
        assert_eq!(clauses[0].1, Query::prefix("message", "conn"));
        assert_eq!(clauses[1].1, Query::wildcard("message", "c*n"));
        assert_eq!(clauses[2].1, Query::phrase("message", &["quick", "brown"]));
        // BOOL 混合 occur 节点
        let q = parse_bool_sexpr(
            "(BOOL (MUST (TERM message a)) (SHOULD (TERM level INFO)) (NOT (TERM message b)))",
        )
        .unwrap();
        let Query::Bool { clauses } = &q else {
            panic!("top must be Bool");
        };
        assert_eq!(
            clauses.iter().map(|(o, _)| *o).collect::<Vec<_>>(),
            vec![Occur::Must, Occur::Should, Occur::MustNot]
        );
        // 错误：未闭合 / 未知头 / 空 AND / 空 BOOL / 多余 token / NOT 多子句 / 非法 occur
        assert!(parse_bool_sexpr("(AND (TERM f a)").is_err());
        assert!(parse_bool_sexpr("(XOR (TERM f a))").is_err());
        assert!(parse_bool_sexpr("(AND)").is_err());
        assert!(parse_bool_sexpr("(BOOL)").is_err());
        assert!(parse_bool_sexpr("(TERM f a) junk").is_err());
        assert!(parse_bool_sexpr("(NOT (TERM f a) (TERM f b))").is_err());
        assert!(parse_bool_sexpr("(BOOL (FILTER (TERM f a)))").is_err());
    }
}
```

跑：`cargo test -p rustlucene-core --bin rustlucene-cli parse_bool_sexpr` —— 预期 FAIL（`parse_bool_sexpr` 未定义）。

实现。（a）`rustlucene-cli.rs` 的 search use 行改为：

```rust
use rustlucene_core::search::{CountCollector, Occur, Query, Searcher};
```

（b）`doc_csv` 函数之后插入解析器（RANGE 叶子在 Step 9 接入；此前 RANGE 落 `unknown node head` 错误）：

```rust
/// M6 §2.6 BOOL 行 S 表达式解析（grammar 见 M6 计划 Task A「查询文件
/// 格式」一节；与 SearchBench.parseBoolSexpr 同一 grammar）。单行、行内
/// 无 tab、节点间恰一个空格；AND/OR/NOT 是同形 BOOL 的语法糖，BOOL 节点
/// 支持混合 occur。RANGE 叶子（T-B PointRange）在 T-B 合入后接入。
fn parse_bool_sexpr(s: &str) -> Result<Query, String> {
    let spaced = s.replace('(', " ( ").replace(')', " ) ");
    let toks: Vec<&str> = spaced.split_whitespace().collect();
    let mut pos = 0;
    let q = sexpr_node(&toks, &mut pos)?;
    if pos != toks.len() {
        return Err(format!("trailing tokens at {pos} of {}", toks.len()));
    }
    Ok(q)
}

fn sexpr_atom<'t>(toks: &[&'t str], pos: &mut usize) -> Result<&'t str, String> {
    let t = toks
        .get(*pos)
        .copied()
        .ok_or_else(|| "unexpected end of sexpr".to_string())?;
    if t == "(" || t == ")" {
        return Err(format!("expected atom, found '{t}'"));
    }
    *pos += 1;
    Ok(t)
}

fn sexpr_close(toks: &[&str], pos: &mut usize) -> Result<(), String> {
    if toks.get(*pos) != Some(&")") {
        return Err(format!("expected ')' at token {pos}"));
    }
    *pos += 1;
    Ok(())
}

fn sexpr_node(toks: &[&str], pos: &mut usize) -> Result<Query, String> {
    if toks.get(*pos) != Some(&"(") {
        return Err(format!("expected '(' at token {pos}"));
    }
    *pos += 1;
    let head = sexpr_atom(toks, pos)?;
    match head {
        "TERM" => {
            let field = sexpr_atom(toks, pos)?;
            let term = sexpr_atom(toks, pos)?;
            sexpr_close(toks, pos)?;
            Ok(Query::term(field, term))
        }
        "PREFIX" => {
            let field = sexpr_atom(toks, pos)?;
            let prefix = sexpr_atom(toks, pos)?;
            sexpr_close(toks, pos)?;
            Ok(Query::prefix(field, prefix))
        }
        "WILDCARD" => {
            let field = sexpr_atom(toks, pos)?;
            let pattern = sexpr_atom(toks, pos)?;
            sexpr_close(toks, pos)?;
            Ok(Query::wildcard(field, pattern))
        }
        "PHRASE" => {
            let field = sexpr_atom(toks, pos)?;
            let t1 = sexpr_atom(toks, pos)?;
            let t2 = sexpr_atom(toks, pos)?;
            sexpr_close(toks, pos)?;
            Ok(Query::phrase(field, &[t1, t2]))
        }
        "AND" | "OR" => {
            let occur = if head == "AND" {
                Occur::Must
            } else {
                Occur::Should
            };
            let mut clauses = Vec::new();
            loop {
                match toks.get(*pos) {
                    Some(&")") => break,
                    Some(_) => clauses.push((occur, sexpr_node(toks, pos)?)),
                    None => return Err(format!("unclosed {head} node")),
                }
            }
            sexpr_close(toks, pos)?;
            if clauses.is_empty() {
                return Err(format!("{head} needs at least one child"));
            }
            Ok(Query::bool(clauses))
        }
        "NOT" => {
            let sub = sexpr_node(toks, pos)?;
            sexpr_close(toks, pos)?;
            Ok(Query::bool(vec![(Occur::MustNot, sub)]))
        }
        "BOOL" => {
            let mut clauses = Vec::new();
            loop {
                match toks.get(*pos) {
                    Some(&")") => break,
                    Some(&"(") => {
                        *pos += 1;
                        let occ = match sexpr_atom(toks, pos)? {
                            "MUST" => Occur::Must,
                            "SHOULD" => Occur::Should,
                            "NOT" => Occur::MustNot,
                            other => {
                                return Err(format!(
                                    "BOOL clause occur must be MUST/SHOULD/NOT, found '{other}'"
                                ))
                            }
                        };
                        let sub = sexpr_node(toks, pos)?;
                        sexpr_close(toks, pos)?;
                        clauses.push((occ, sub));
                    }
                    Some(t) => return Err(format!("expected clause, found '{t}'")),
                    None => return Err("unclosed BOOL node".to_string()),
                }
            }
            sexpr_close(toks, pos)?;
            if clauses.is_empty() {
                return Err("BOOL needs at least one clause".to_string());
            }
            Ok(Query::bool(clauses))
        }
        other => Err(format!("unknown node head '{other}'")),
    }
}
```

（c）`searchbench` 内 `phrase_tasks` 解析块之后插入 BOOL 行解析：

```rust
    // M6 BOOL 行（S 表达式单行，与 Java SearchBench 同格式逐条重放）：
    // (bucket, sexpr, parsed)。解析失败即 fail-fast（查询文件两侧同源，
    // 坏行是电池 bug 不是数据问题）。
    let bool_tasks: Vec<(String, String, Query)> = content
        .lines()
        .filter(|l| l.starts_with("BOOL\t"))
        .map(|l| {
            let parts: Vec<&str> = l.splitn(3, '\t').collect();
            if parts.len() < 3 {
                eprintln!("searchbench: malformed BOOL line: {l}");
                std::process::exit(2);
            }
            let q = parse_bool_sexpr(parts[2]).unwrap_or_else(|e| {
                eprintln!("searchbench: bad BOOL sexpr '{}': {e}", parts[2]);
                std::process::exit(2);
            });
            (parts[1].to_string(), parts[2].to_string(), q)
        })
        .collect();
```

`WorkItem` 枚举加变体：

```rust
        Bool(String, Query), // (sexpr, parsed)——sexpr 进 correctness detail 行
```

work 列表追加（`phrase_tasks` 循环之后）：

```rust
    for (bucket, sexpr, q) in &bool_tasks {
        work.push((
            format!("bool\t{bucket}"),
            WorkItem::Bool(sexpr.clone(), q.clone()),
        ));
    }
```

`build_query` 与 `detail_of` 各加一臂：

```rust
            WorkItem::Bool(_, q) => q.clone(),
```
```rust
            WorkItem::Bool(sexpr, _) => format!("bool={sexpr} bucket={label}"),
```

（detail 行格式与 Java `SearchBench` 的 `"bool=" + sexpr + " bucket=bool\t" + bucket` 逐字一致——逐条 diff 的前提。）

跑：`cargo test -p rustlucene-core --bin rustlucene-cli` —— 解析测试 PASS；`cargo test -p rustlucene-core` 全量 PASS。

提交：

```bash
cargo fmt && cargo test -p rustlucene-core
git add crates/core/src/bin/rustlucene-cli.rs
git commit -m "feat: searchbench BOOL 行 S 表达式解析与逐条重放（Rust 侧，spec §2.6）"
```

- [ ] **Step 9: BOOL 行 RANGE 叶子 → `Query::PointRange`（需 T-B 已合入）**

**依赖**：T-B 的 `Query::PointRange { field: String, low: i64, high: i64 }` 已合入 main。若 T-A 先完成，本步暂缓到 T-B 落地后执行（T-A 其余步骤无此依赖）。

先写失败测试（`rustlucene-cli.rs` 的 `tests` 模块内追加）：

```rust
    /// RANGE 叶子 → Query::PointRange（钉死的跨任务接口，T-B 交付执行）。
    #[test]
    fn parse_bool_sexpr_range_leaf() {
        let q = parse_bool_sexpr(
            "(AND (TERM level INFO) (NOT (RANGE timestamp 1700000000000 1700000100000)))",
        )
        .unwrap();
        let Query::Bool { clauses } = &q else {
            panic!("top must be Bool");
        };
        let Query::Bool { clauses: not_clauses } = &clauses[1].1 else {
            panic!("NOT child must be Bool");
        };
        assert_eq!(
            not_clauses[0].1,
            Query::PointRange {
                field: "timestamp".to_string(),
                low: 1_700_000_000_000,
                high: 1_700_000_100_000,
            }
        );
        assert!(parse_bool_sexpr("(RANGE timestamp abc 5)").is_err());
    }
```

跑：`cargo test -p rustlucene-core --bin rustlucene-cli parse_bool_sexpr_range_leaf` —— 预期 FAIL（RANGE 落 `unknown node head`）。

实现（`sexpr_node` 的 match 中 `"PHRASE"` 臂之后插入）：

```rust
        "RANGE" => {
            let field = sexpr_atom(toks, pos)?;
            let low: i64 = sexpr_atom(toks, pos)?
                .parse()
                .map_err(|_| "RANGE low must be a decimal i64".to_string())?;
            let high: i64 = sexpr_atom(toks, pos)?
                .parse()
                .map_err(|_| "RANGE high must be a decimal i64".to_string())?;
            sexpr_close(toks, pos)?;
            Ok(Query::PointRange {
                field: field.to_string(),
                low,
                high,
            })
        }
```

跑：`cargo test -p rustlucene-core --bin rustlucene-cli` —— PASS。

提交：

```bash
cargo fmt && cargo test -p rustlucene-core
git add crates/core/src/bin/rustlucene-cli.rs
git commit -m "feat: BOOL 行 RANGE 叶子——映射 Query::PointRange（T-B 接口）"
```

- [ ] **Step 10: SearchBench.java BOOL 行解析 + dump 生成（Java 侧）**

Java 侧无失败测试框架，验证 = 编译 + 功能 smoke（dump 产 BOOL 行、load 重放成功）。

实现。（a）`interop/java/SearchBench.java` 第 5 行 import 块加：

```java
import org.apache.lucene.document.LongPoint;
```

（b）`serialiseQuery` 方法之后插入解析器（package-static，`VerifySearchIndex` 复用）：

```java
    /**
     * M6 BOOL 行 S 表达式解析（与 rustlucene-cli parse_bool_sexpr 同一
     * grammar，见 M6 计划 Task A「查询文件格式」一节）。叶子包
     * ConstantScoreQuery（与既有 AND/OR 行同款）；OR 节点显式 msm=1；
     * BOOL 混合节点：无 MUST 且有 SHOULD 时 msm=1（= Lucene 默认化简，
     * 显式钉死），有 MUST 时 SHOULD 纯可选（msm=0）。
     */
    static Query parseBoolSexpr(String s) {
        String spaced = s.replace("(", " ( ").replace(")", " ) ").trim();
        List<String> toks = new ArrayList<>();
        for (String t : spaced.split("\\s+")) toks.add(t);
        int[] pos = {0};
        Query q = parseBoolNode(toks, pos);
        if (pos[0] != toks.size())
            throw new IllegalArgumentException("trailing tokens at " + pos[0]);
        return q;
    }

    static String sexprAtom(List<String> toks, int[] pos) {
        if (pos[0] >= toks.size())
            throw new IllegalArgumentException("unexpected end of sexpr");
        String t = toks.get(pos[0]);
        if (t.equals("(") || t.equals(")"))
            throw new IllegalArgumentException("expected atom, found '" + t + "'");
        pos[0]++;
        return t;
    }

    static void sexprClose(List<String> toks, int[] pos) {
        if (pos[0] >= toks.size() || !toks.get(pos[0]).equals(")"))
            throw new IllegalArgumentException("expected ')' at token " + pos[0]);
        pos[0]++;
    }

    static Query parseBoolNode(List<String> toks, int[] pos) {
        if (pos[0] >= toks.size() || !toks.get(pos[0]).equals("("))
            throw new IllegalArgumentException("expected '(' at token " + pos[0]);
        pos[0]++;
        String head = sexprAtom(toks, pos);
        switch (head) {
            case "TERM": {
                String f = sexprAtom(toks, pos), t = sexprAtom(toks, pos);
                sexprClose(toks, pos);
                return new ConstantScoreQuery(new TermQuery(new Term(f, t)));
            }
            case "PREFIX": {
                String f = sexprAtom(toks, pos), p = sexprAtom(toks, pos);
                sexprClose(toks, pos);
                return new ConstantScoreQuery(new PrefixQuery(new Term(f, p)));
            }
            case "WILDCARD": {
                String f = sexprAtom(toks, pos), p = sexprAtom(toks, pos);
                sexprClose(toks, pos);
                return new ConstantScoreQuery(new WildcardQuery(new Term(f, p)));
            }
            case "PHRASE": {
                String f = sexprAtom(toks, pos), t1 = sexprAtom(toks, pos), t2 = sexprAtom(toks, pos);
                sexprClose(toks, pos);
                return new ConstantScoreQuery(new PhraseQuery(f, t1, t2));
            }
            case "RANGE": {
                String f = sexprAtom(toks, pos);
                long lo = Long.parseLong(sexprAtom(toks, pos));
                long hi = Long.parseLong(sexprAtom(toks, pos));
                sexprClose(toks, pos);
                return new ConstantScoreQuery(LongPoint.newRangeQuery(f, lo, hi));
            }
            case "AND": case "OR": {
                BooleanClause.Occur occur = head.equals("AND")
                        ? BooleanClause.Occur.MUST : BooleanClause.Occur.SHOULD;
                BooleanQuery.Builder b = new BooleanQuery.Builder();
                int n = 0;
                while (pos[0] < toks.size() && !toks.get(pos[0]).equals(")")) {
                    b.add(parseBoolNode(toks, pos), occur);
                    n++;
                }
                sexprClose(toks, pos);
                if (n == 0) throw new IllegalArgumentException(head + " needs at least one child");
                if (occur == BooleanClause.Occur.SHOULD) b.setMinimumNumberShouldMatch(1);
                return new ConstantScoreQuery(b.build());
            }
            case "NOT": {
                Query sub = parseBoolNode(toks, pos);
                sexprClose(toks, pos);
                BooleanQuery.Builder b = new BooleanQuery.Builder();
                b.add(sub, BooleanClause.Occur.MUST_NOT);
                return new ConstantScoreQuery(b.build());
            }
            case "BOOL": {
                BooleanQuery.Builder b = new BooleanQuery.Builder();
                boolean hasMust = false, hasShould = false;
                int n = 0;
                while (pos[0] < toks.size() && !toks.get(pos[0]).equals(")")) {
                    if (!toks.get(pos[0]).equals("("))
                        throw new IllegalArgumentException("expected clause at token " + pos[0]);
                    pos[0]++;
                    String occ = sexprAtom(toks, pos);
                    BooleanClause.Occur occur;
                    switch (occ) {
                        case "MUST": occur = BooleanClause.Occur.MUST; hasMust = true; break;
                        case "SHOULD": occur = BooleanClause.Occur.SHOULD; hasShould = true; break;
                        case "NOT": occur = BooleanClause.Occur.MUST_NOT; break;
                        default: throw new IllegalArgumentException(
                                "BOOL clause occur must be MUST/SHOULD/NOT, found '" + occ + "'");
                    }
                    b.add(parseBoolNode(toks, pos), occur);
                    sexprClose(toks, pos);
                    n++;
                }
                sexprClose(toks, pos);
                if (n == 0) throw new IllegalArgumentException("BOOL needs at least one clause");
                if (hasShould && !hasMust) b.setMinimumNumberShouldMatch(1);
                return new ConstantScoreQuery(b.build());
            }
            default:
                throw new IllegalArgumentException("unknown node head '" + head + "'");
        }
    }
```

（c）`--load-queries` 分支：声明区加 `List<String[]> loadedBoolSexpr = new ArrayList<>();`；行解析循环加一臂：

```java
                    } else if (parts[0].equals("BOOL") && parts.length >= 3) {
                        loadedBoolSexpr.add(new String[]{parts[1], parts[2]});
                    }
```

重放块（`loadedPhrase` 循环之后）加：

```java
                // M6 BOOL lines, replayed verbatim like the AND/OR lines
                for (String[] p : loadedBoolSexpr) {
                    queries.add(parseBoolSexpr(p[1]));
                    labels.add("bool\t" + p[0]);
                    details.add("bool=" + p[1] + " bucket=bool\t" + p[0]);
                }
```

（d）`--dump-queries` 分支：AND/OR 生成循环之后插入 BOOL 生成（四种确定性形状轮换；拍平 AND/OR 覆盖 Rust 拍平路径的 A/B，两个嵌套形覆盖通用组合器）：

```java
                    // M6 nested BOOL lines (S 表达式, spec M6 §2.6): 四种
                    // 确定性形状——拍平 AND / 拍平 OR（同字段纯 Term，Rust
                    // 侧命中 roaring 三档）与 MUST(OR)+NOT / 三层
                    // OR(AND(NOT))（通用组合器路径）。
                    for (FreqBucket bucket : FreqBucket.values()) {
                        List<TermStats> sample = buckets.get(bucket);
                        if (sample.size() < 4) continue;
                        for (int i = 0; i < tasks; i++) {
                            String t1 = sample.get(rng.nextInt(sample.size())).term.utf8ToString();
                            String t2 = sample.get(rng.nextInt(sample.size())).term.utf8ToString();
                            String t3 = sample.get(rng.nextInt(sample.size())).term.utf8ToString();
                            String t4 = sample.get(rng.nextInt(sample.size())).term.utf8ToString();
                            String sexpr;
                            switch (i % 4) {
                                case 0:
                                    sexpr = "(AND (TERM " + field + " " + t1 + ") (TERM " + field + " " + t2
                                            + ") (TERM " + field + " " + t3 + "))";
                                    break;
                                case 1:
                                    sexpr = "(OR (TERM " + field + " " + t1 + ") (TERM " + field + " " + t2
                                            + ") (TERM " + field + " " + t3 + "))";
                                    break;
                                case 2:
                                    sexpr = "(AND (TERM " + field + " " + t1 + ") (OR (TERM " + field + " " + t2
                                            + ") (TERM " + field + " " + t3 + ")) (NOT (TERM " + field + " " + t4 + ")))";
                                    break;
                                default:
                                    sexpr = "(OR (TERM " + field + " " + t1 + ") (AND (TERM " + field + " " + t2
                                            + ") (NOT (TERM " + field + " " + t3 + "))))";
                                    break;
                            }
                            pw.printf(Locale.ROOT, "BOOL\t%s\t%s%n", bucketLabel(bucket), sexpr);
                        }
                    }
```

（e）类 javadoc 的查询文件格式清单加一行：`BOOL\t<bucket>\t<sexpr>   (nested BooleanQuery, S-expression; see M6 spec §2.6)`。

编译 + smoke（javac 无输出即成功；命令照 `Makefile:9-11` java-classes 目标）：

```bash
javac -cp "interop/java/lib/lucene-core-9.12.3.jar:interop/java/lib/lucene-analysis-common-9.12.3.jar" \
  -d interop/java/classes interop/java/*.java
CP="interop/java/classes:interop/java/lib/lucene-core-9.12.3.jar:interop/java/lib/lucene-analysis-common-9.12.3.jar"
rm -rf /tmp/rl-m6-smoke && java -cp "$CP" JavaLogBench /tmp/rl-m6-smoke 20000 1 42
java -cp "$CP" SearchBench /tmp/rl-m6-smoke message --dump-queries /tmp/m6-smoke-q.txt --tasks 5 --seed 42
grep -c '^BOOL' /tmp/m6-smoke-q.txt   # 预期 >0（每 bucket 5 条 × 非空 bucket 数）
java -cp "$CP" SearchBench /tmp/rl-m6-smoke message \
  --load-queries /tmp/m6-smoke-q.txt --warmup 1 --iter 2 --no-cache > /dev/null
grep -c '^bool=' <(java -cp "$CP" SearchBench /tmp/rl-m6-smoke message \
  --load-queries /tmp/m6-smoke-q.txt --warmup 1 --iter 2 --no-cache 2>&1 > /dev/null)   # 预期 = 上一条 grep 数
```

提交：

```bash
git add interop/java/SearchBench.java
git commit -m "feat: SearchBench BOOL 行解析 + --load-queries 重放 + --dump-queries 生成（Java 侧，spec §2.6）"
```

- [ ] **Step 11: searchdump / VerifySearchIndex 嵌套 Bool 电池（make log-test 接入）**

spec §2.6「`make log-test` 电池接入」：verify-log.sh / verify-search.sh **零改动**——电池硬编码两侧镜像，diff 走既有全量输出对拍；`--bitmap` 变体的 `RL_BITMAP=0` searchdump A/B（verify-log.sh:43-49）自动覆盖 Bool 拍平 A/B。

（a）`crates/core/src/bin/rustlucene-cli.rs` `searchdump`：wildcard 电池循环之后、phrase 电池之前插入：

```rust
    // M6 §2.6 nested-Bool battery: spec §2 三态 + 拍平形 + 嵌套 + 跨字段
    // + Prefix/Wildcard 子句；S 表达式即 searchbench BOOL 行格式。
    // Mirrored in VerifySearchIndex.java.
    let bool_battery: [&str; 8] = [
        "(AND (TERM message connection0) (TERM message query23))", // 纯 MUST（拍平形）
        "(OR (TERM level INFO) (TERM level WARN) (TERM level ERROR))", // 纯 SHOULD（拍平形）
        "(BOOL (MUST (TERM message connection0)) (SHOULD (TERM level INFO)))", // MUST+SHOULD 丢弃
        "(AND (TERM level INFO) (NOT (TERM message connection0)))", // MUST+MUST_NOT 跨字段
        "(NOT (TERM level WARN))",                                 // 纯 MUST_NOT
        "(OR (TERM message connection0) (AND (TERM level ERROR) (NOT (TERM message query23))))", // 三层嵌套跨字段
        "(AND (PREFIX message conn) (NOT (TERM level WARN)))",     // Prefix 子句
        "(BOOL (MUST (WILDCARD message que?y3*)) (SHOULD (TERM level DEBUG)))", // Wildcard + 混合
    ];
    for sexpr in bool_battery {
        let q = parse_bool_sexpr(sexpr)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let count = searcher.count(&q)?;
        let (_, docs) = searcher.top_docs(&q, 20)?;
        out.push_str(&format!(
            "bool {sexpr} count={count} first20={}\n",
            doc_csv(&docs)
        ));
    }
```

phrase 电池的 `if positions && num_docs > 7` 块内（`degenerate` 循环之后，`t0`/`t1` 仍在作用域）追加 phrase 子句项：

```rust
        // M6 Bool + Phrase 子句（positions variant only）
        let bool_phrase = format!("(AND (PHRASE message {t0} {t1}) (NOT (TERM level WARN)))");
        let q = parse_bool_sexpr(&bool_phrase)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let count = searcher.count(&q)?;
        let (_, docs) = searcher.top_docs(&q, 20)?;
        out.push_str(&format!(
            "bool {bool_phrase} count={count} first20={}\n",
            doc_csv(&docs)
        ));
```

（b）`interop/java/VerifySearchIndex.java`：wildcard 电池循环之后、phrase 电池之前插入镜像（解析复用 `SearchBench.parseBoolSexpr`，同 package）：

```java
            // M6 nested-Bool battery: same items/format as the bool_battery
            // in rustlucene-cli searchdump.
            String[] boolBattery = {
                "(AND (TERM message connection0) (TERM message query23))",
                "(OR (TERM level INFO) (TERM level WARN) (TERM level ERROR))",
                "(BOOL (MUST (TERM message connection0)) (SHOULD (TERM level INFO)))",
                "(AND (TERM level INFO) (NOT (TERM message connection0)))",
                "(NOT (TERM level WARN))",
                "(OR (TERM message connection0) (AND (TERM level ERROR) (NOT (TERM message query23))))",
                "(AND (PREFIX message conn) (NOT (TERM level WARN)))",
                "(BOOL (MUST (WILDCARD message que?y3*)) (SHOULD (TERM level DEBUG)))",
            };
            for (String sexpr : boolBattery) {
                Query q = SearchBench.parseBoolSexpr(sexpr);
                TopDocs td = s.search(q, 20, Sort.INDEXORDER);
                StringBuilder b = new StringBuilder();
                for (ScoreDoc sd : td.scoreDocs) b.append(sd.doc).append(',');
                out.append("bool ").append(sexpr)
                   .append(" count=").append(s.count(q))
                   .append(" first20=").append(b).append('\n');
            }
```

phrase 电池块内（`degenerate` 循环之后）追加：

```java
                {
                    String boolPhrase = "(AND (PHRASE message " + t0 + " " + t1
                                        + ") (NOT (TERM level WARN)))";
                    Query q = SearchBench.parseBoolSexpr(boolPhrase);
                    TopDocs td = s.search(q, 20, Sort.INDEXORDER);
                    StringBuilder b = new StringBuilder();
                    for (ScoreDoc sd : td.scoreDocs) b.append(sd.doc).append(',');
                    out.append("bool ").append(boolPhrase)
                       .append(" count=").append(s.count(q))
                       .append(" first20=").append(b).append('\n');
                }
```

编译 + 两个快速变体对拍（全量 `make log-test` 留给 Step 12）：

```bash
cargo build --release -p rustlucene-core
javac -cp "interop/java/lib/lucene-core-9.12.3.jar:interop/java/lib/lucene-analysis-common-9.12.3.jar" \
  -d interop/java/classes interop/java/*.java
interop/verify-log.sh 50000 42
interop/verify-log.sh 50000 43 --positions
```

预期：两个变体均输出 `LOG_INTEROP_OK`（中间 diff 全为空；`--positions` 变体含 Bool+Phrase 行）。

提交：

```bash
git add crates/core/src/bin/rustlucene-cli.rs interop/java/VerifySearchIndex.java
git commit -m "test: searchdump/VerifySearchIndex 嵌套 Bool 电池——三态/拍平/嵌套/跨字段/混合子句（make log-test 接入）"
```

- [ ] **Step 12: 验收（spec §2.6 全清单，无 commit）**

- [ ] Rust 全量单测：`cargo test -p rustlucene-core`（含 Step 1-7 全部 Bool 测试）
- [ ] 全量 interop 电池：`make log-test`（5 变体全绿；`--bitmap` 变体含 `RL_BITMAP=0` searchdump A/B——Bool 拍平形 on/off 逐位一致）
- [ ] searchbench BOOL 行三路 hit counts 逐条 diff（m3 报告口径）：

```bash
CP="interop/java/classes:interop/java/lib/lucene-core-9.12.3.jar:interop/java/lib/lucene-analysis-common-9.12.3.jar"
cargo build --release -p rustlucene-core --bin rustlucene-cli
# 两侧同语料索引（bitmap + positions，拍平 A/B 与 phrase 子句的前提）
cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
  logwrite /tmp/rl-m6-rust 1000000 42 --positions --bitmap
java -cp "$CP" JavaLogBench /tmp/rl-m6-java 1000000 1 42 --positions
java -cp "$CP" SearchBench /tmp/rl-m6-java message \
  --dump-queries /tmp/m6-q.txt --tasks 50 --seed 42
grep -c '^BOOL' /tmp/m6-q.txt   # 预期 ≥100（非空 bucket × 50）
# 三路 replay（stderr = 逐条 hit counts）
cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
  searchbench /tmp/rl-m6-rust message --load-queries /tmp/m6-q.txt --warmup 10 --iter 30 \
  > /tmp/m6-bench-rust.out 2> /tmp/m6-counts-rust.txt
RL_BITMAP=0 cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
  searchbench /tmp/rl-m6-rust message --load-queries /tmp/m6-q.txt --warmup 10 --iter 30 \
  > /tmp/m6-bench-pfor.out 2> /tmp/m6-counts-pfor.txt
java -cp "$CP" SearchBench /tmp/rl-m6-java message \
  --load-queries /tmp/m6-q.txt --no-cache --warmup 10 --iter 30 \
  > /tmp/m6-bench-java.out 2> /tmp/m6-counts-java.txt
# BOOL 行逐条 diff：Rust-roaring vs Java / Rust-roaring vs Rust-PFOR（拍平 A/B）
grep '^bool=' /tmp/m6-counts-rust.txt | sort > /tmp/m6-bool-rust.txt
grep '^bool=' /tmp/m6-counts-java.txt | sort > /tmp/m6-bool-java.txt
grep '^bool=' /tmp/m6-counts-pfor.txt | sort > /tmp/m6-bool-pfor.txt
diff -u /tmp/m6-bool-rust.txt /tmp/m6-bool-java.txt   # 期望：无输出
diff -u /tmp/m6-bool-rust.txt /tmp/m6-bool-pfor.txt   # 期望：无输出
# 全类型回归（既有行类型不受 Bool 接入影响）
diff -u <(grep -v '^#' /tmp/m6-counts-rust.txt | sort) \
        <(grep -v '^#' /tmp/m6-counts-java.txt | sort)   # 期望：无输出
```

- [ ] spec §2.6 形状覆盖核对清单（逐条 ↔ 覆盖点）：
  - 纯 MUST → searchdump 电池第 1 项 + Step 5 `flat_and` + dump 形状 0（拍平 roaring 路径）
  - 纯 SHOULD → 电池第 2 项 + Step 5 `flat_or` + dump 形状 1（拍平 roaring 路径）
  - MUST+SHOULD（丢弃语义）→ 电池第 3 项（`(BOOL (MUST …) (SHOULD …))`，Java 同语义 msm=0）+ Step 4 语义测试锚点 6
  - MUST+MUST_NOT → 电池第 4 项 + Step 4 锚点 `{5,10,15}`
  - 纯 MUST_NOT → 电池第 5 项 + Step 4 锚点 14/20（含缺失 term MatchAll）+ Step 5 锚点 4286
  - 三层嵌套 → 电池第 6 项 + Step 4 锚点 `{2,5,7,10,12,15,17}` + dump 形状 3
  - 跨字段 → 电池第 4/6/7 项 + Step 4 跨字段锚点
  - Phrase/Prefix/Wildcard 子句 → 电池第 7/8 项 + positions 变体 phrase 行 + Step 5 `bool_query_mixed_clause_types`（PointRange 子句由 T-B 电池覆盖，spec §3.4）
  - 拍平形 roaring 路径 A/B（`RL_BITMAP=0`）→ Step 5 变体断言 + on/off 等价 + 本步三路 diff 第 2 条
- [ ] 记账：`.superpowers/sdd/progress.md` 记 Task A 完成 + review 结论（gitignored）

---

### Task B: Point 区间查询（1D BKD 读路径）

**Files:**

- Create: `crates/codec-lucene9/src/points_read.rs`（BKD 读路径全部代码 + `#[cfg(test)]` 测试模块）
- Create: `crates/codec-lucene9/src/roaring/materialized.rs`（`MaterializedBitmap`）
- Modify: `crates/codec-lucene9/src/points.rs`（`pub(crate)` 可见性开放：三个 codec 名常量、`FORMAT_VERSION`、`BKD_CODEC_NAME`、`BKD_VERSION`、`get_num_left_leaf_nodes`——全部生产代码需要，非 test-only）
- Modify: `crates/codec-lucene9/src/roaring.rs`（`mod materialized;` + re-export）
- Modify: `crates/codec-lucene9/src/lib.rs`（`pub mod points_read;`）
- Modify: `crates/core/src/search/segment_reader.rs`（`points: Option<PointsReader>` 挂载）
- Modify: `crates/core/src/search/doc_iter.rs`（`DocsBitmap` trait、`BitmapCursor<B>` 泛型化、`PointsDocIter`、`SegmentDocIter::Points` 变体）
- Modify: `crates/core/src/search/query.rs`（`Query::PointRange` 变体 + `point_range_bitmap` 共享物化入口）
- Modify: `crates/core/src/search/searcher.rs`（count cardinality 快路径、`freq_sum` 拒绝 PointRange）
- Modify: `crates/core/src/search/mod.rs`（PointRange 行为测试）
- Modify: `crates/core/src/bin/rustlucene-cli.rs`（searchbench RANGE 行解析 + searchdump range 电池行）
- Modify: `interop/java/SearchBench.java`（RANGE 行解析 + `LongPoint`/`IntPoint.newRangeQuery` 构造）
- Modify: `interop/java/VerifySearchIndex.java`（range 电池行）
- Test: `crates/codec-lucene9/src/points_read.rs` 的 `#[cfg(test)] mod tests`（先用 `PointsWriter` 造段再读回，postings_read.rs 同式）

**Interfaces:**

- Consumes（跨任务钉死接口的消费侧）: `Query::PointRange { field: String, low: i64, high: i64 }`；`crate::points::{file_names, MAX_POINTS_IN_LEAF_NODE, get_num_left_leaf_nodes}` + codec 名常量（本任务改 `pub(crate)`）；`FieldInfos::{by_name, by_number}`；`FSDirectory::{open_input, open_checksum_input, file_exists}`；`IndexInput::slice` + `DataInput` primitives；`codec_util::{check_header, check_index_header, check_footer, check_footer_structure, corrupt}`；`croaring::Bitmap`（`of(&[u32])`、`cardinality()`、`iter()`——与 `roaring/frozen.rs` 既有用法同款，零新 unsafe）。
- Produces（T-A/T-C 依赖这些名字与契约，不得改名）:

  ```rust
  // codec-lucene9/src/points_read.rs（跨任务钉死签名）
  pub struct PointsReader { .. }
  impl PointsReader {
      pub fn open(dir: &FSDirectory, segment: &str, segment_id: &[u8; 16],
                  field_infos: &FieldInfos)
          -> io::Result<Option<PointsReader>>;        // 段无 points 文件 → None
      pub fn intersect(&self, field: &str, low: i64, high: i64,
                       visitor: &mut dyn FnMut(i64, i32)) -> io::Result<()>;
      // visitor 收 (packed_value_as_i64, doc_id)；多值点逐值回调、调用方去重；
      // 未知字段 / 非 point 字段 → 零回调（Lucene null-scorer 语义）
  }
  // codec-lucene9/src/roaring/materialized.rs
  pub struct MaterializedBitmap { .. }
  impl MaterializedBitmap {
      pub fn of(sorted_dedup_docs: &[u32]) -> MaterializedBitmap;
      pub fn cardinality(&self) -> u64;
      pub fn docs_from(&self, from: u32, dst: &mut [u32]) -> usize;
  }
  // core/search/doc_iter.rs
  pub trait DocsBitmap { fn docs_from(&self, from: u32, dst: &mut [u32]) -> usize; }
  pub struct PointsDocIter { .. }              // DocIter；freq() = 1（ConstantScore）
  SegmentDocIter::Points(PointsDocIter)        // 新变体
  // core/search/query.rs
  Query::PointRange { field: String, low: i64, high: i64 }   // 钉死；low>high → Err(InvalidInput)
  pub(crate) fn point_range_bitmap(seg: &mut SegmentReader, field: &str,
                                   low: i64, high: i64)
      -> io::Result<Option<MaterializedBitmap>>;  // segment_iterator 与 count 共用
  ```

- T-C 消费方式：`intersect(field, i64::MIN, i64::MAX, visitor)` 即全量 `(value, doc)` 遍历（spec §4.2 points 行），root cell 全 Inside 时逐叶全解码不过滤，天然吸收多值点。
- 接口备注（评审已采纳修订）：钉死签名初稿的 `open` 不带 `segment_id`，评审拍板补入第三参数 `segment_id: &[u8; 16]`——三个文件（`.kdm`/`.kdi`/`.kdd`）照 codec 既有惯例（`postings_read.rs:35-90`、`terms_read.rs:92`）逐个 `check_index_header(codec, 0, 0, segment_id, "")` 校验，无降级；本章节全部代码与调用点均已按修订后签名书写。

**设计要点（Steps 前先读）:**

1. **1D 全部比较在解包后的 `i64` 上做**。sortable 字节序 == 有符号 i64 序（`NumericUtils.longToSortableBytes` :210-214 翻符号位 + 大端），所以 `PointRangeQuery.relate`（search/PointRangeQuery.java:145-167）的无符号字节比较在 1D 下与 `i64` 三次比较有完全相同的结果；叶内逐点过滤同理（`matches` :130-143）。IntPoint（4 字节）零填充进高 4 字节（points.rs:70-76），解包用 `sortableBytesToInt`（NumericUtils.java:198-202）再宽化。
2. **整叶收分支与 Lucene `addAll` 的偏差（有意）**。Lucene 对 CELL_INSIDE 子树走 `visitDocIDs → addAll`（BKDReader.java:557-586）只解 docs block、不解 values；本实现 visitor 钉死 `(value, doc)` 双参（T-C 全量取点也要 value），所以 Inside 分支**逐叶全量解码但跳过逐点过滤**。I/O 节省的大头——整棵不相交子树不读 `.kdd`（叶 fp 全部来自内存 packed index，seek 精确制导）——完整保留；docs-only 快路径留作后续独立 API（YAGNI，本里程碑无消费者）。
3. **物化而非增量遍历**（spec §3.3）：段内命中经 visitor 收集 → `sort_unstable` + `dedup`（多值点去重 + BKD 访问序按 `(value, doc)` 非 doc 序，两步同时解决）→ `MaterializedBitmap`。count = cardinality 直读；Bool 组合按普通子迭代器进 T-A 组合器；**不参与 roaring 拍平**（spec §2.4 拍平仅限同字段 Term 叶子）。croaring 类型不出 codec（M5 关键设计事实 7）：core 只见 `DocsBitmap` trait。
4. **`low>high` 行为的事实修正**：spec §3.4 称"low>high 与 Lucene 一致直接报错"——核实 9.12.3 源码后此说不实：`PointRangeQuery.checkArgs`（:100-110）只查 null，`newRangeQuery` 对 low>high 不抛异常，BKD 走查自然全 Outside → **Java 返回 0 命中**。跨任务钉死接口已定为 `Err(InvalidInput)`，本章节照此实现（Rust 侧显式错误面），但该情形**不进 Java diff 电池**（无法对拍），由 Rust 单测锁定。
5. **packed index 全量驻留**：open 时把每字段 `.kdi` 的 packed index 一次解码成 `leaf_fps`（leaves_offset 序）+ `splits`（split_offset = right_offset−1 序）两张表（1D 树小：每叶摊 ~2 个 VInt/VLong 字节）。树形状契约与写侧共享 `get_num_left_leaf_nodes`（BKDWriter.java:831-847 ↔ BKDReader size :519-521），节点递归即 `readNodeData`（:657-717）的前序展开——points.rs 测试模块（:918-1046）已按字节验证过同一套解码逻辑，本任务把它提升为生产代码（游标换 `DataInput`，`assert!` 换 `corrupt`）。
6. codec 测试构造法与 postings_read.rs 一致：`PointsWriter` + `FieldInfos::write` 造段 → `PointsReader::open` 读回；叶级断言直接调私有 `read_leaf`（测试模块同文件，可见私有项），行为级断言 brute-force 对照。

#### Steps

- [ ] **Step B.1: codec `PointsReader::open`——.kdm 解析 + packed index 全量解码（先失败测试）**

  新建 `crates/codec-lucene9/src/points_read.rs`，先只写模块骨架 + 测试模块（编译失败：`open`/`FieldMeta` 尚不存在）：

  ```rust
  //! Lucene 9.12.3-compatible 1D Points (BKD tree) reader
  //! (`codecs/lucene90/Lucene90PointsReader.java`): `.kdm` per-field metadata,
  //! `.kdi` packed inner-node index (fully resident at open), `.kdd` leaf
  //! blocks read on demand. Reverse of `points.rs` (the writer); format
  //! citations point at `util/bkd/BKDReader.java` / `util/bkd/DocIdsWriter.java`.
  //!
  //! Scope mirrors the writer: numDims == numIndexDims == 1, long (8) and
  //! int (4) bytes-per-dim. All comparisons run on unpacked i64 values:
  //! sortable-byte unsigned order == signed i64 order
  //! (NumericUtils.longToSortableBytes :210-214), so the 1D cell relation
  //! (PointRangeQuery.relate :145-167) on unpacked values is identical to
  //! Lucene's unsigned byte compare.
  //!
  //! Deviation (deliberate): CELL_INSIDE subtrees decode whole leaves
  //! including values (Lucene's addAll :562-586 decodes only doc ids) —
  //! the pinned visitor carries (value, doc) for T-C point enumeration.
  //! The dominant I/O saving is preserved: disjoint subtrees are never
  //! read from `.kdd` (leaf fps come from the in-memory packed index).
  //!
  //! Header checks follow the codec read-side convention
  //! (postings_read.rs:35-90): each of the three files goes through
  //! `check_index_header(codec, VERSION, VERSION, segment_id, "")` at open
  //! (Lucene90PointsReader :63-93).

  use std::io;

  use crate::codec_util::{
      check_footer, check_footer_structure, check_header, check_index_header, corrupt,
  };
  use crate::directory::FSDirectory;
  use crate::field_infos::FieldInfos;
  use crate::io::{ChecksumIndexInput, DataInput, IndexInput};
  use crate::points::{
      BKD_CODEC_NAME, BKD_VERSION, DATA_CODEC_NAME, FORMAT_VERSION, INDEX_CODEC_NAME,
      MAX_POINTS_IN_LEAF_NODE, META_CODEC_NAME, file_names, get_num_left_leaf_nodes,
  };

  #[cfg(test)]
  mod tests {
      use super::*;
      use crate::field_infos::FieldInfo;
      use crate::points::PointsWriter;
      use std::collections::BTreeSet;
      use std::fs;
      use std::path::PathBuf;

      // ---------- deterministic RNG (SplitMix64, points.rs 同式) ----------

      struct Rng(u64);

      impl Rng {
          fn next(&mut self) -> u64 {
              self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
              let mut z = self.0;
              z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
              z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
              z ^ (z >> 31)
          }
          fn below(&mut self, n: u64) -> u64 {
              self.next() % n
          }
      }

      fn temp_dir(tag: &str) -> PathBuf {
          let dir = std::env::temp_dir().join(format!(
              "codec-lucene9-pointsread-{}-{}",
              tag,
              std::process::id()
          ));
          let _ = fs::remove_dir_all(&dir);
          dir
      }

      /// PointsWriter 造段 + 配套 .fnm（postings_read.rs 测试同式：先写后读）。
      /// `long_fields`/`int_fields`: (field_number, field_name, points)。
      fn write_segment(
          tag: &str,
          long_fields: &[(i32, &str, Vec<(i64, u32)>)],
          int_fields: &[(i32, &str, Vec<(i32, u32)>)],
      ) -> (PathBuf, FieldInfos) {
          let root = temp_dir(tag);
          let dir = FSDirectory::open(&root).unwrap();
          let seg_id = [7u8; 16];
          let mut fis_vec = Vec::new();
          for &(number, name, _) in long_fields {
              let mut fi = FieldInfo::stored(name, number);
              fi.point_dimension_count = 1;
              fi.point_index_dimension_count = 1;
              fi.point_num_bytes = 8;
              fis_vec.push(fi);
          }
          for &(number, name, _) in int_fields {
              let mut fi = FieldInfo::stored(name, number);
              fi.point_dimension_count = 1;
              fi.point_index_dimension_count = 1;
              fi.point_num_bytes = 4;
              fis_vec.push(fi);
          }
          let fis = FieldInfos::new(fis_vec);
          fis.write(&dir, "_0", &seg_id, "").unwrap();
          let mut w = PointsWriter::new(&dir, "_0", &seg_id).unwrap();
          for (number, _, pts) in long_fields {
              w.write_field_long(*number, &mut pts.clone()).unwrap();
          }
          for (number, _, pts) in int_fields {
              w.write_field_int(*number, &mut pts.clone()).unwrap();
          }
          w.finish().unwrap();
          (root, fis)
      }

      fn open(root: &PathBuf, fis: &FieldInfos) -> PointsReader {
          let dir = FSDirectory::open(root).unwrap();
          PointsReader::open(&dir, "_0", &[7u8; 16], fis).unwrap().expect("has points")
      }

      fn gen_long_points(rng: &mut Rng, n: usize, doc_range: u64) -> Vec<(i64, u32)> {
          (0..n)
              .map(|_| {
                  let value = match rng.below(6) {
                      0 => (rng.below(1_000)) as i64,
                      1 => (rng.below(100)) as i64 - 50,
                      2 => rng.next() as i64,
                      3 => [i64::MIN, i64::MAX, 0, -1, 1][rng.below(5) as usize],
                      4 => (rng.next() % 1_000_000) as i64,
                      _ => (rng.below(10)) as i64,
                  };
                  (value, rng.below(doc_range) as u32)
              })
              .collect()
      }

      #[test]
      fn open_parses_meta_and_decodes_index() {
          // 1200 点 → 3 叶（512+512+176）；doc 0..1200 全 distinct
          let points: Vec<(i64, u32)> = (0..1200u32).map(|i| (i as i64 * 7 - 3000, i)).collect();
          let (root, fis) = write_segment("open-meta", &[(0, "ts", points.clone())], &[]);
          let reader = open(&root, &fis);
          assert_eq!(reader.fields.len(), 1);
          let (name, m) = &reader.fields[0];
          assert_eq!(name, "ts");
          assert_eq!(m.field_number, 0);
          assert_eq!(m.bytes_per_dim, 8);
          assert_eq!(m.num_leaves, 3);
          assert_eq!(m.point_count, 1200);
          assert_eq!(m.doc_count, 1200);
          assert_eq!(m.min_value, -3000);
          assert_eq!(m.max_value, 1199 * 7 - 3000);
          // leaf fp 递增、首叶 fp == dataStartFP
          assert_eq!(m.leaf_fps.len(), 3);
          assert_eq!(m.leaf_fps[0], m.data_start_fp);
          assert!(m.leaf_fps[0] < m.leaf_fps[1] && m.leaf_fps[1] < m.leaf_fps[2]);
          // splits（split_offset 序）== 叶 1/叶 2 的首值（写侧 leaf_block_start_values,
          // points.rs:182-199）；值按 (value, doc) 排序后叶 i 首点即第 512*i 个点
          let mut sorted = points.clone();
          sorted.sort();
          assert_eq!(m.splits, vec![sorted[512].0, sorted[1024].0]);
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn open_multi_field_and_doc_count_dedup() {
          let mut rng = Rng(11);
          let longs = gen_long_points(&mut rng, 5000, 3000); // 多值点 + 重复值
          let ints: Vec<(i32, u32)> = (0..700u32).map(|i| ((i % 97) as i32, i / 2)).collect();
          let (root, fis) = write_segment(
              "open-multi",
              &[(2, "ts", longs.clone())],
              &[(5, "lvl", ints.clone())],
          );
          let reader = open(&root, &fis);
          assert_eq!(reader.fields.len(), 2);
          let (n0, m0) = &reader.fields[0];
          assert_eq!((n0.as_str(), m0.field_number, m0.bytes_per_dim), ("ts", 2, 8));
          assert_eq!(m0.point_count, 5000);
          assert_eq!(
              m0.doc_count as usize,
              longs.iter().map(|p| p.1).collect::<BTreeSet<_>>().len(),
              "docCount counts distinct docs (BKDWriter :1256)"
          );
          assert_eq!(m0.num_leaves, 10); // ceil(5000/512)
          assert_eq!(m0.splits.len(), 9);
          let (n1, m1) = &reader.fields[1];
          assert_eq!((n1.as_str(), m1.field_number, m1.bytes_per_dim), ("lvl", 5, 4));
          assert_eq!(m1.num_leaves, 2); // ceil(700/512)
          assert_eq!(m1.min_value, 0);
          assert_eq!(m1.max_value, 96);
          // 第二个字段的 dataStartFP 紧随第一个字段的数据（fields tightly packed）
          assert!(m1.data_start_fp > m0.data_start_fp);
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn open_single_leaf_tree() {
          let points: Vec<(i64, u32)> = (0..100u32).map(|i| (i as i64, i)).collect();
          let (root, fis) = write_segment("open-single", &[(0, "ts", points)], &[]);
          let reader = open(&root, &fis);
          let (_, m) = &reader.fields[0];
          assert_eq!(m.num_leaves, 1);
          assert!(m.splits.is_empty());
          assert_eq!(m.leaf_fps.len(), 1);
          assert_eq!(m.leaf_fps[0], m.data_start_fp);
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn open_without_points_returns_none() {
          // 无 .kdm 的段目录 → Ok(None)（写侧只在有 point 数据时创建三文件，
          // segment_builder.rs:240-245）
          let root = temp_dir("open-none");
          let dir = FSDirectory::open(&root).unwrap();
          let fis = FieldInfos::new(vec![FieldInfo::stored("body", 0)]);
          assert!(PointsReader::open(&dir, "_0", &[7u8; 16], &fis).unwrap().is_none());
          fs::remove_dir_all(&root).unwrap();
      }
  }
  ```

  同时做 `points.rs` 可见性开放（生产代码需要，3 处单行改）：

  ```rust
  // points.rs:30-34 三个 codec 名常量与 FORMAT_VERSION:
  pub(crate) const DATA_CODEC_NAME: &str = "Lucene90PointsFormatData";
  pub(crate) const INDEX_CODEC_NAME: &str = "Lucene90PointsFormatIndex";
  pub(crate) const META_CODEC_NAME: &str = "Lucene90PointsFormatMeta";
  pub(crate) const FORMAT_VERSION: u32 = 0;
  // points.rs:43-44:
  pub(crate) const BKD_CODEC_NAME: &str = "BKD";
  pub(crate) const BKD_VERSION: u32 = 9;
  // points.rs:447:
  pub(crate) fn get_num_left_leaf_nodes(num_leaves: usize) -> usize { /* body 不变 */ }
  ```

  `lib.rs` 模块注册（`pub mod points;` 之后一行）：

  ```rust
  pub mod points_read;
  ```

  跑测试确认失败：

  ```
  $ cargo test -p codec-lucene9 points_read 2>&1 | tail -4
  error[E0412]: cannot find type `PointsReader` in this scope
  error[E0433]: use of unresolved `PointsReader` ...（open / FieldMeta / decode_packed_index 均未实现）
  ```

- [ ] **Step B.2: `PointsReader::open` 实现** — 在骨架的 `use` 块后、`#[cfg(test)]` 前写入（本步骤主体一：.kdm 元数据解析 + .kdi 全量驻留解码）：

  ```rust
  /// One field's `.kdm` entry + fully decoded packed index (BKDReader ctor
  /// :56-113 + BKDPointTree readNodeData :657-717, decoded eagerly at open).
  struct FieldMeta {
      field_number: i32,
      bytes_per_dim: usize,
      num_leaves: usize,
      min_value: i64,
      max_value: i64,
      point_count: u64,
      doc_count: u32,
      data_start_fp: u64,
      index_start_fp: u64,
      packed_index_byte_length: usize,
      /// Leaf block fp per leaf, in leaves_offset (in-order) sequence.
      leaf_fps: Vec<u64>,
      /// Split value per inner-node boundary, indexed by
      /// `split_offset == right_offset - 1`; len == num_leaves - 1.
      splits: Vec<i64>,
  }

  /// Per-segment points reader: `.kdd` random-access stream + one fully
  /// resident packed index per point field (1D trees are tiny).
  /// `Ok(None)` from `open` when the segment has no points files.
  pub struct PointsReader {
      data_in: IndexInput,
      /// (field name, meta) — names resolved from `field_infos` at open.
      fields: Vec<(String, FieldMeta)>,
  }

  /// NumericUtils.sortableBytesToLong (:221-227) / sortableBytesToInt
  /// (:198-202; the writer zero-pads int values into the high 4 bytes,
  /// points.rs:70-76): packed big-endian sortable bytes → signed value.
  fn unpack_value(packed: &[u8; 8], bytes_per_dim: usize) -> i64 {
      if bytes_per_dim == 8 {
          (u64::from_be_bytes(*packed) ^ 0x8000_0000_0000_0000) as i64
      } else {
          debug_assert_eq!(bytes_per_dim, 4);
          ((u32::from_be_bytes(packed[..4].try_into().unwrap()) ^ 0x8000_0000) as i32) as i64
      }
  }

  impl PointsReader {
      /// Lucene90PointsReader ctor (:41-123): opens the three files, checks
      /// each index header against `segment_id` (CodecUtil.checkIndexHeader
      /// :246-258, postings_read.rs:35-90 convention), parses every `.kdm`
      /// field entry, reconciles the recorded `.kdi`/`.kdd` lengths, and
      /// decodes each field's packed index fully into memory.
      pub fn open(
          dir: &FSDirectory,
          segment: &str,
          segment_id: &[u8; 16],
          field_infos: &FieldInfos,
      ) -> io::Result<Option<PointsReader>> {
          let [data_name, index_name, meta_name] = file_names(segment);
          if !dir.file_exists(&meta_name) {
              return Ok(None);
          }

          // .kdm: full sequential checksum read (Lucene90PointsReader :83-112)
          let mut meta_in = dir.open_checksum_input(&meta_name)?;
          check_index_header(
              &mut meta_in,
              META_CODEC_NAME,
              FORMAT_VERSION,
              FORMAT_VERSION,
              segment_id,
              "",
          )?;
          let mut fields: Vec<(String, FieldMeta)> = Vec::new();
          loop {
              let field_number = meta_in.read_int()?; // :96
              if field_number == -1 {
                  break;
              }
              if field_number < 0 {
                  return Err(corrupt(format!("illegal field number {field_number} (:99-100)")));
              }
              fields.push(read_field_meta(&mut meta_in, field_infos, field_number)?);
          }
          // footer reconciliation (:105-106)
          let index_length = meta_in.read_long()? as u64;
          let data_length = meta_in.read_long()? as u64;
          check_footer(&mut meta_in)?;

          // .kdi: header + recorded length; each field's packed index is
          // sliced out and decoded eagerly (retrieveChecksum :115)
          let mut index_in = dir.open_input(&index_name)?;
          check_index_header(
              &mut index_in,
              INDEX_CODEC_NAME,
              FORMAT_VERSION,
              FORMAT_VERSION,
              segment_id,
              "",
          )?;
          check_footer_structure(&index_in, index_length)?;
          for (_, m) in &mut fields {
              let packed =
                  index_in.slice(m.index_start_fp, m.packed_index_byte_length as u64)?;
              decode_packed_index(packed, m)?;
          }

          // .kdd: header + recorded length; leaves are random-access reads (:116)
          let mut data_in = dir.open_input(&data_name)?;
          check_index_header(
              &mut data_in,
              DATA_CODEC_NAME,
              FORMAT_VERSION,
              FORMAT_VERSION,
              segment_id,
              "",
          )?;
          check_footer_structure(&data_in, data_length)?;

          Ok(Some(PointsReader { data_in, fields }))
      }
  }

  /// One `.kdm` field entry (Lucene90PointsWriter.writeField :144-148 +
  /// BKDWriter finalizer :1236-1264, reversed; BKDReader ctor :56-113).
  fn read_field_meta(
      meta_in: &mut ChecksumIndexInput,
      field_infos: &FieldInfos,
      field_number: i32,
  ) -> io::Result<(String, FieldMeta)> {
      // CodecUtil.checkHeader("BKD", 9) (:57-59); our writer always emits
      // VERSION_CURRENT == VERSION_META_FILE == 9 (BKDWriter.java:88-89)
      check_header(meta_in, BKD_CODEC_NAME, BKD_VERSION, BKD_VERSION)?;
      let num_dims = meta_in.read_vint()?; // :60
      let num_index_dims = meta_in.read_vint()?; // :63 (version >= SELECTIVE_INDEXING)
      if num_dims != 1 || num_index_dims != 1 {
          return Err(corrupt(format!(
              "only 1D points are supported: numDims={num_dims} numIndexDims={num_index_dims}"
          )));
      }
      let max_points_in_leaf = meta_in.read_vint()?; // :67
      if max_points_in_leaf as usize != MAX_POINTS_IN_LEAF_NODE {
          return Err(corrupt(format!(
              "maxPointsInLeafNode {max_points_in_leaf} != {MAX_POINTS_IN_LEAF_NODE}"
          )));
      }
      let bytes_per_dim = meta_in.read_vint()? as usize; // :68
      if bytes_per_dim != 4 && bytes_per_dim != 8 {
          return Err(corrupt(format!("unsupported bytesPerDim {bytes_per_dim}")));
      }
      let num_leaves = meta_in.read_vint()? as usize; // :72
      if num_leaves == 0 {
          return Err(corrupt("numLeaves == 0"));
      }
      let mut min_packed = [0u8; 8]; // :75-79
      meta_in.read_bytes(&mut min_packed[..bytes_per_dim])?;
      let mut max_packed = [0u8; 8];
      meta_in.read_bytes(&mut max_packed[..bytes_per_dim])?;
      if min_packed[..bytes_per_dim] > max_packed[..bytes_per_dim] {
          return Err(corrupt("minPackedValue > maxPackedValue (:82-95)"));
      }
      let point_count = meta_in.read_vlong()? as u64; // :97
      let doc_count = meta_in.read_vint()? as u32; // :98
      let packed_index_byte_length = meta_in.read_vint()? as usize; // :100
      let data_start_fp = meta_in.read_long()? as u64; // :102 (version >= META_FILE)
      let index_start_fp = meta_in.read_long()? as u64; // :103

      // 字段名解析 + 与 .fnm 交叉校验（Lucene90PointsReader.getValues
      // :131-141 的写侧对照：.kdm 条目一定来自有数据的 point 字段）
      let fi = field_infos
          .by_number(field_number)
          .ok_or_else(|| corrupt(format!("points field number {field_number} not in .fnm")))?;
      if fi.point_dimension_count != 1 || fi.point_num_bytes as usize != bytes_per_dim {
          return Err(corrupt(format!(
              "field {}: .fnm point config (dims={}, bytes={}) != .kdm entry (bytes={bytes_per_dim})",
              fi.name, fi.point_dimension_count, fi.point_num_bytes
          )));
      }

      Ok((
          fi.name.clone(),
          FieldMeta {
              field_number,
              bytes_per_dim,
              num_leaves,
              min_value: unpack_value(&min_packed, bytes_per_dim),
              max_value: unpack_value(&max_packed, bytes_per_dim),
              point_count,
              doc_count,
              data_start_fp,
              index_start_fp,
              packed_index_byte_length,
              leaf_fps: vec![0; num_leaves],
              splits: vec![0; num_leaves - 1],
          },
      ))
  }

  /// BKDPointTree packed-index decode (readNodeData :657-717 + the pre-order
  /// child recursion :253-311), filling `leaf_fps` / `splits` in place.
  /// Consumes the packed bytes exactly; trailing bytes are corruption.
  fn decode_packed_index(mut input: IndexInput, m: &mut FieldMeta) -> io::Result<()> {
      // BKDPointTree ctor: nodeID=1, isLeft=false, minBlockFP=0, lastSplitValues
      // all-zero, negativeDeltas all-false (:253-254; writer packIndex :1025-1037)
      decode_node(&mut input, m, 0, [0u8; 8], false, false, 0, m.num_leaves)?;
      if input.file_pointer() != input.length() {
          return Err(corrupt("packed index has trailing bytes"));
      }
      Ok(())
  }

  /// readNodeData (:657-717) for one node covering
  /// `leaves_offset..leaves_offset + num_leaves`, then pre-order recursion
  /// into children. `min_block_fp` / `last_split_value` / `negative_delta`
  /// are leafBlockFPStack[level-1] / splitValuesStack[level-1] /
  /// negativeDeltas as set by the parent (:658-684); `total_num_leaves`
  /// (the reader's leafNodeOffset, :481-483) is `m.num_leaves`.
  #[allow(clippy::too_many_arguments)]
  fn decode_node(
      input: &mut IndexInput,
      m: &mut FieldMeta,
      min_block_fp: u64,
      last_split_value: [u8; 8],
      negative_delta: bool,
      is_left: bool,
      leaves_offset: usize,
      num_leaves: usize,
  ) -> io::Result<()> {
      // leafBlockFPStack[level] = stack[level-1] (+ VLong delta if right) (:658-662)
      let mut fp = min_block_fp;
      if !is_left {
          fp += input.read_vlong()? as u64;
      }
      if num_leaves == 1 {
          m.leaf_fps[leaves_offset] = fp;
          return Ok(());
      }

      let code = input.read_vint()?; // :687
      // numIndexDims == 1 ⇒ splitDim == 0 and code stays whole (:688-690)
      let prefix = (code % (1 + m.bytes_per_dim as i32)) as usize; // :691
      let suffix = m.bytes_per_dim - prefix; // :692
      // splitValuesStack[level] starts as a copy of the parent's (:675-684)
      let mut split_value = last_split_value;
      if suffix > 0 {
          let mut first_diff_byte_delta = code / (1 + m.bytes_per_dim as i32); // :695
          if negative_delta {
              first_diff_byte_delta = -first_diff_byte_delta; // :696-698
          }
          let old_byte = i32::from(split_value[prefix]); // :700
          split_value[prefix] = (old_byte + first_diff_byte_delta) as u8; // :701
          input.read_bytes(&mut split_value[prefix + 1..prefix + 1 + (suffix - 1)])?; // :702
      }
      // else: split == last split on this dim (many duplicate values) (:703-706)

      let num_left = get_num_left_leaf_nodes(num_leaves);
      let right_offset = leaves_offset + num_left;
      m.splits[right_offset - 1] = unpack_value(&split_value, m.bytes_per_dim);

      // leftNumBytes present iff the left child is an inner node
      // (nodeID*2 < leafNodeOffset ⇔ num_left > 1) (:708-713)
      let left_num_bytes = if num_left > 1 {
          input.read_vint()? as u64 // :709
      } else {
          0
      };
      // rightNodePositions[level] (:714): the right subtree follows the left one
      let right_node_position = input.file_pointer() + left_num_bytes;
      decode_node(input, m, fp, split_value, true, true, leaves_offset, num_left)?;
      if input.file_pointer() != right_node_position {
          return Err(corrupt("leftNumBytes measures a different left subtree"));
      }
      decode_node(
          input,
          m,
          fp,
          split_value,
          false,
          false,
          right_offset,
          num_leaves - num_left,
      )?;
      Ok(())
  }
  ```

  跑测试确认全绿 + fmt + commit：

  ```
  $ cargo test -p codec-lucene9 points_read 2>&1 | tail -3
  test result: ok. 4 passed; 0 failed
  $ cargo test -p codec-lucene9 2>&1 | tail -2    # points.rs 可见性改动不影响既有测试
  test result: ok. ... passed; 0 failed
  $ cargo fmt --check && echo FMT_OK
  FMT_OK
  $ git add -A && git commit -m "feat: points_read — .kdm meta parse + packed index eager decode (PointsReader::open)"
  ```

- [ ] **Step B.3: 叶解码 + `intersect` 递归三分支（先失败测试）**

  在 points_read.rs 测试模块追加（编译失败：`intersect`/`read_leaf` 尚不存在）：

  ```rust
      // ---------- 行为级辅助：collect + brute force ----------

      fn collect(reader: &PointsReader, field: &str, low: i64, high: i64) -> Vec<(i64, i32)> {
          let mut hits = Vec::new();
          reader
              .intersect(field, low, high, &mut |v, d| hits.push((v, d)))
              .unwrap();
          hits.sort();
          hits
      }

      fn brute_force(points: &[(i64, u32)], low: i64, high: i64) -> Vec<(i64, i32)> {
          let mut v: Vec<(i64, i32)> = points
              .iter()
              .filter(|&&(val, _)| val >= low && val <= high)
              .map(|&(val, d)| (val, d as i32))
              .collect();
          v.sort();
          v
      }

      fn field_meta<'a>(reader: &'a PointsReader, field: &str) -> &'a FieldMeta {
          &reader
              .fields
              .iter()
              .find(|(n, _)| n == field)
              .unwrap_or_else(|| panic!("field {field} exists"))
              .1
      }

      /// 写侧按 (value, doc) 排序后 512 一切（points.rs:174-205）——期望的叶序列。
      fn expected_leaves(points: &[(i64, u32)]) -> Vec<Vec<(i64, u32)>> {
          let mut sorted = points.to_vec();
          sorted.sort();
          sorted
              .chunks(MAX_POINTS_IN_LEAF_NODE)
              .map(|c| c.to_vec())
              .collect()
      }

      // ---------- 叶解码（read_leaf 直调，五分支 doc ids 全覆盖） ----------

      #[test]
      fn read_leaf_matches_writer_layout() {
          // 数据形态即写侧五分支测试的形态（points.rs:1417-1461）：
          // continuous（连续 doc）、bitset（稀疏严格序）、delta16（重复 doc
          // 小跨度 / 稀疏大步）、bpv24（doc 跨度 > 0xFFFF）、bpv32（> 0xFFFFFF）
          let cases: Vec<(&str, Vec<(i64, u32)>)> = vec![
              ("continuous", (0..2000u32).map(|i| (i as i64 * 7, i)).collect()),
              ("bitset", (0..2000u32).map(|i| (i as i64, i * 3)).collect()),
              (
                  "delta16",
                  (0..2000u32).map(|i| (i as i64, (i / 2) as u32)).collect(),
              ),
              (
                  "bpv24",
                  gen_long_points(&mut Rng(42), 5000, 1_000_000),
              ),
              (
                  "bpv32",
                  gen_long_points(&mut Rng(43), 5000, 1_000_000_000),
              ),
          ];
          for (tag, points) in cases {
              let (root, fis) = write_segment(tag, &[(0, "ts", points.clone())], &[]);
              let reader = open(&root, &fis);
              let m = field_meta(&reader, "ts");
              let expected = expected_leaves(&points);
              assert_eq!(m.num_leaves, expected.len(), "{tag}");
              for (i, want) in expected.iter().enumerate() {
                  assert_eq!(reader.read_leaf(m, i).unwrap(), *want, "{tag} leaf {i}");
              }
              fs::remove_dir_all(&root).unwrap();
          }
      }

      #[test]
      fn read_leaf_all_equal_and_int() {
          // 全等值叶（compressedDim -1 分支）+ 分裂 delta 0 链
          let points: Vec<(i64, u32)> = (0..1200u32).map(|doc| (777, doc)).collect();
          let (root, fis) = write_segment("leaf-equal", &[(0, "ts", points.clone())], &[]);
          let reader = open(&root, &fis);
          let m = field_meta(&reader, "ts");
          assert_eq!(m.splits, vec![777, 777]);
          for (i, want) in expected_leaves(&points).iter().enumerate() {
              assert_eq!(reader.read_leaf(m, i).unwrap(), *want, "leaf {i}");
          }
          fs::remove_dir_all(&root).unwrap();
          // int 字段：i32 值解包宽化为 i64
          let ints: Vec<(i32, u32)> = vec![
              (i32::MIN, 3),
              (0, 1),
              (i32::MAX, 2),
              (-1, 5),
              (1, 4),
          ];
          let (root, fis) = write_segment("leaf-int", &[], &[(5, "lvl", ints)]);
          let reader = open(&root, &fis);
          let m = field_meta(&reader, "lvl");
          let got = reader.read_leaf(m, 0).unwrap();
          let mut want: Vec<(i64, u32)> = vec![
              (i32::MIN as i64, 3),
              (-1, 5),
              (0, 1),
              (1, 4),
              (i32::MAX as i64, 2),
          ];
          want.sort();
          assert_eq!(got, want);
          fs::remove_dir_all(&root).unwrap();
      }

      // ---------- intersect：三分支 + 边界 + clamp + 多值点 ----------

      #[test]
      fn intersect_matches_brute_force_multileaf() {
          let mut rng = Rng(0xC0FFEE);
          let points = gen_long_points(&mut rng, 5000, 3000); // 10 叶
          let (root, fis) = write_segment("intersect-5k", &[(0, "ts", points.clone())], &[]);
          let reader = open(&root, &fis);
          let sorted_vals: Vec<i64> = {
              let mut v: Vec<i64> = points.iter().map(|p| p.0).collect();
              v.sort();
              v
          };
          let (lo, mid, hi) = (sorted_vals[1000], sorted_vals[2500], sorted_vals[4000]);
          for (low, high) in [
              (i64::MIN, i64::MAX), // 全区间：root 直接 Inside
              (mid, mid),           // 点查询退化 [v,v]
              (lo, hi),             // 中部区间：三分支都打
              (hi + 1, hi + 1000),  // 可能不相交
              (i64::MIN, lo),       // 贴 MIN
              (hi, i64::MAX),       // 贴 MAX
              (1_000_000, 1_000_000), // 生成器值域内单点
          ] {
              assert_eq!(
                  collect(&reader, "ts", low, high),
                  brute_force(&points, low, high),
                  "range [{low}, {high}]"
              );
          }
          // 保证不相交（生成器 full-range 分支是 u64 转 i64，不保证留出空隙，
          // 用全空值域之外的区间锁不相交路径）
          assert!(collect(&reader, "ts", i64::MAX - 1, i64::MAX)
              .iter()
              .all(|&(v, _)| v >= i64::MAX - 1));
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn intersect_disjoint_is_empty() {
          let points: Vec<(i64, u32)> = (0..1000u32).map(|i| (i as i64, i)).collect();
          let (root, fis) = write_segment("intersect-disjoint", &[(0, "ts", points)], &[]);
          let reader = open(&root, &fis);
          assert!(collect(&reader, "ts", 2000, 3000).is_empty());
          assert!(collect(&reader, "ts", -100, -1).is_empty());
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn intersect_multivalued_docs_callback_per_value() {
          // 多值点逐值回调（spec §3.2：调用方去重，codec 不去重）
          let points: Vec<(i64, u32)> = vec![(5, 3), (50, 3), (500, 3), (70, 8), (5, 8)];
          let (root, fis) = write_segment("intersect-multi", &[(0, "ts", points)], &[]);
          let reader = open(&root, &fis);
          assert_eq!(
              collect(&reader, "ts", i64::MIN, i64::MAX),
              vec![(5, 3), (5, 8), (50, 3), (70, 8), (500, 3)]
          );
          assert_eq!(collect(&reader, "ts", 5, 5), vec![(5, 3), (5, 8)]);
          assert_eq!(collect(&reader, "ts", 6, 100), vec![(50, 3), (70, 8)]);
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn intersect_int_field_clamps_bounds() {
          let ints: Vec<(i32, u32)> = (0..100u32).map(|i| (i as i32 - 50, i)).collect();
          let (root, fis) = write_segment("intersect-int", &[], &[(5, "lvl", ints.clone())]);
          let reader = open(&root, &fis);
          let as_i64: Vec<(i64, u32)> = ints.iter().map(|&(v, d)| (v as i64, d)).collect();
          // i64 全域 → clamp 到 i32 全域
          assert_eq!(
              collect(&reader, "lvl", i64::MIN, i64::MAX),
              brute_force(&as_i64, i64::MIN, i64::MAX)
          );
          // 部分出界 clamp
          assert_eq!(
              collect(&reader, "lvl", i64::MIN, -40),
              brute_force(&as_i64, -50, -40)
          );
          // 整区间出 i32 域 → 零回调（spec §3.1 clamp-to-empty）
          assert!(collect(&reader, "lvl", i32::MAX as i64 + 1, i64::MAX).is_empty());
          assert!(collect(&reader, "lvl", i64::MIN, i32::MIN as i64 - 1).is_empty());
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn intersect_boundaries_min_max() {
          let points: Vec<(i64, u32)> = vec![
              (i64::MIN, 0),
              (i64::MIN + 1, 1),
              (-1, 2),
              (0, 3),
              (1, 4),
              (i64::MAX - 1, 5),
              (i64::MAX, 6),
          ];
          let (root, fis) = write_segment("intersect-minmax", &[(0, "ts", points.clone())], &[]);
          let reader = open(&root, &fis);
          for (low, high) in [
              (i64::MIN, i64::MIN),
              (i64::MAX, i64::MAX),
              (i64::MIN, -1),
              (1, i64::MAX),
              (i64::MIN, i64::MAX),
          ] {
              assert_eq!(
                  collect(&reader, "ts", low, high),
                  brute_force(&points, low, high),
                  "range [{low}, {high}]"
              );
          }
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn intersect_unknown_field_is_empty() {
          let points: Vec<(i64, u32)> = vec![(1, 0)];
          let (root, fis) = write_segment("intersect-unknown", &[(0, "ts", points)], &[]);
          let reader = open(&root, &fis);
          assert!(collect(&reader, "nope", i64::MIN, i64::MAX).is_empty());
          fs::remove_dir_all(&root).unwrap();
      }
  ```

  跑测试确认失败：

  ```
  $ cargo test -p codec-lucene9 points_read 2>&1 | tail -4
  error[E0599]: no method named `intersect` found for struct `PointsReader`
  error[E0599]: no method named `read_leaf` found ...
  ```

- [ ] **Step B.4: 叶解码 + `intersect` 实现** — 在 `decode_node` 之后写入（本任务主体二：.kdd 叶按需读 + DocIdsWriter 五分支反向解码 + 节点 min/max 三分支）：

  ```rust
  /// PointValues.Relation (:223-230).
  #[derive(Clone, Copy, PartialEq, Eq, Debug)]
  enum Relation {
      Inside,
      Outside,
      Crosses,
  }

  /// PointRangeQuery.relate (:145-167) specialized to numDims == 1: the
  /// unsigned byte compares degenerate to signed i64 compares on unpacked
  /// values (sortable byte order == signed order).
  fn relate(cell_lo: i64, cell_hi: i64, low: i64, high: i64) -> Relation {
      if cell_lo > high || cell_hi < low {
          Relation::Outside // :152-155
      } else if cell_lo < low || cell_hi > high {
          Relation::Crosses // :157-163
      } else {
          Relation::Inside // :164-166
      }
  }

  impl PointsReader {
      /// PointValues.intersect driver (:344-380) for 1D inclusive ranges.
      /// `visitor` receives (value, doc_id) per matching point — multi-valued
      /// docs arrive once per value; dedup is the caller's job (spec §3.2).
      /// `intersect(field, i64::MIN, i64::MAX, ..)` doubles as the forceMerge
      /// point-enumeration channel (spec §4.2).
      ///
      /// `low > high` is tolerated here as an empty result (relate naturally
      /// reports Outside everywhere — Java `newRangeQuery` does not reject it
      /// either, PointRangeQuery.checkArgs :100-110); the `Err(InvalidInput)`
      /// contract lives at the `Query::PointRange` layer (spec §3.1).
      pub fn intersect(
          &self,
          field: &str,
          low: i64,
          high: i64,
          visitor: &mut dyn FnMut(i64, i32),
      ) -> io::Result<()> {
          let Some((_, m)) = self.fields.iter().find(|(name, _)| name == field) else {
              return Ok(()); // 段内无此 point 字段：空命中（null-scorer 语义）
          };
          // IntPoint 复用（spec §3.1）：4 字节字段把查询界 clamp 进 i32 值域；
          // 整区间出界 → 零回调。
          let (low, high) = if m.bytes_per_dim == 4 {
              if low > i32::MAX as i64 || high < i32::MIN as i64 {
                  return Ok(());
              }
              (
                  low.clamp(i32::MIN as i64, i32::MAX as i64),
                  high.clamp(i32::MIN as i64, i32::MAX as i64),
              )
          } else {
              (low, high)
          };
          // root cell == [min_value, max_value]（.kdm min/max packed value）
          self.intersect_node(m, 0, m.num_leaves, m.min_value, m.max_value, low, high, visitor)
      }

      /// PointValues.intersect (:352-380) recursive driver, 1D:
      /// Outside → 跳过（不读 .kdd）；Inside → 整子树逐叶全收（无逐点过滤）；
      /// Crosses → 内部节点按 split 收紧 cell 界递归、叶内逐点过滤。
      #[allow(clippy::too_many_arguments)]
      fn intersect_node(
          &self,
          m: &FieldMeta,
          leaves_offset: usize,
          num_leaves: usize,
          cell_lo: i64,
          cell_hi: i64,
          low: i64,
          high: i64,
          visitor: &mut dyn FnMut(i64, i32),
      ) -> io::Result<()> {
          match relate(cell_lo, cell_hi, low, high) {
              Relation::Outside => Ok(()), // :355-357
              Relation::Inside => {
                  // visitDocIDs 全收 (:358-361)——本实现 visitor 需要 value，
                  // 逐叶全量解码但跳过过滤（与 addAll :562-586 的偏差见模块 doc）
                  for i in 0..num_leaves {
                      self.visit_leaf(m, leaves_offset + i, None, visitor)?;
                  }
                  Ok(())
              }
              Relation::Crosses => {
                  if num_leaves == 1 {
                      // visitDocValues 逐点过滤 (:371-373)
                      self.visit_leaf(m, leaves_offset, Some((low, high)), visitor)
                  } else {
                      let num_left = get_num_left_leaf_nodes(num_leaves);
                      let right_offset = leaves_offset + num_left;
                      let split = m.splits[right_offset - 1];
                      // left cell [lo, split], right cell [split, hi]
                      // (pushBoundsLeft/Right, BKDReader.java:375-426)
                      self.intersect_node(
                          m,
                          leaves_offset,
                          num_left,
                          cell_lo,
                          split,
                          low,
                          high,
                          visitor,
                      )?;
                      self.intersect_node(
                          m,
                          right_offset,
                          num_leaves - num_left,
                          split,
                          cell_hi,
                          low,
                          high,
                          visitor,
                      )?;
                      Ok(())
                  }
              }
          }
      }

      /// One leaf: decode every (value, doc) and callback; the `filter`
      /// (Crosses 叶) drops out-of-range points (PointRangeQuery.matches
      /// :130-143).
      fn visit_leaf(
          &self,
          m: &FieldMeta,
          leaves_offset: usize,
          filter: Option<(i64, i64)>,
          visitor: &mut dyn FnMut(i64, i32),
      ) -> io::Result<()> {
          for (value, doc) in self.read_leaf(m, leaves_offset)? {
              if let Some((low, high)) = filter {
                  if value < low || value > high {
                      continue;
                  }
              }
              visitor(value, doc as i32);
          }
          Ok(())
      }

      /// BKDReader.visitDocValues(fp) (:607-631) + readDocIDs (:633-644):
      /// leaf block = VInt count + docs block + commonPrefix block + values
      /// block (writer points.rs:267-296, reversed). Random access via the
      /// packed index's leaf fp (BKDPointTree.getLeafBlockFP :490-494).
      fn read_leaf(&self, m: &FieldMeta, leaves_offset: usize) -> io::Result<Vec<(i64, u32)>> {
          let fp = m.leaf_fps[leaves_offset];
          let mut input = self.data_in.slice(fp, self.data_in.length() - fp)?;
          let count = input.read_vint()?; // :637
          if count <= 0 || count as usize > MAX_POINTS_IN_LEAF_NODE {
              return Err(corrupt(format!(
                  "leaf point count {count} outside [1, {MAX_POINTS_IN_LEAF_NODE}]"
              )));
          }
          let count = count as usize;
          let docs = read_doc_ids(&mut input, count)?; // :639

          // readCommonPrefixes (:947-957), single dim
          let common_prefix_len = input.read_vint()? as usize;
          if common_prefix_len > m.bytes_per_dim {
              return Err(corrupt(format!(
                  "commonPrefixLen {common_prefix_len} > bytesPerDim {}",
                  m.bytes_per_dim
              )));
          }
          let mut value_base = [0u8; 8];
          input.read_bytes(&mut value_base[..common_prefix_len])?;

          // readCompressedDim (:937-945)
          let compressed_dim = input.read_byte()? as i8;
          let mut points: Vec<(i64, u32)> = Vec::with_capacity(count);
          match compressed_dim {
              -1 => {
                  // visitUniqueRawDocValues (:893-901): the common prefix IS the value
                  let v = unpack_value(&value_base, m.bytes_per_dim);
                  for &doc in &docs {
                      points.push((v, doc));
                  }
              }
              0 => {
                  // visitCompressedDocValues (:903-935), 1D: run-length on the
                  // byte at compressedByteOffset == commonPrefixLen (:914-916)
                  if common_prefix_len == m.bytes_per_dim {
                      return Err(corrupt("compressedDim 0 with a full common prefix"));
                  }
                  let suffix_len = m.bytes_per_dim - common_prefix_len - 1;
                  let mut i = 0usize;
                  while i < count {
                      let run_byte = input.read_byte()?; // :919
                      let run_len = input.read_byte()? as usize; // :920
                      if run_len == 0 || i + run_len > count {
                          return Err(corrupt(format!(
                              "bad run {run_len} at point {i}/{count} (:931-934)"
                          )));
                      }
                      for j in 0..run_len {
                          let mut v = value_base;
                          v[common_prefix_len] = run_byte;
                          input.read_bytes(
                              &mut v[common_prefix_len + 1..common_prefix_len + 1 + suffix_len],
                          )?; // :922-927
                          points.push((unpack_value(&v, m.bytes_per_dim), docs[i + j]));
                      }
                      i += run_len;
                  }
              }
              d => {
                  return Err(corrupt(format!(
                      "unsupported compressedDim {d} (the low-cardinality -2 branch is never \
                       emitted by this system's writer, points.rs:284-294)"
                  )));
              }
          }
          Ok(points)
      }
  }

  /// DocIdsWriter.readInts (:182-206): all five write-side branches reversed
  /// (writer points.rs:339-410). Flags -2/-1/16/24/32 (DocIdsWriter.java:30-34);
  /// 0 (LEGACY_DELTA_VINT, :36) is never emitted by 9.x writers and rejected.
  fn read_doc_ids(input: &mut impl DataInput, count: usize) -> io::Result<Vec<u32>> {
      debug_assert!(count > 0);
      let mut docs = vec![0u32; count];
      match input.read_byte()? as i8 {
          -2 => {
              // readContinuousIds (:217-222)
              let start = input.read_vint()? as u32;
              for (i, d) in docs.iter_mut().enumerate() {
                  *d = start.wrapping_add(i as u32);
              }
          }
          -1 => {
              // readBitSet (:233-240) via readBitSetIterator (:208-215)
              let offset_words = input.read_vint()? as u64;
              let word_count = input.read_vint()? as usize;
              let mut pos = 0usize;
              for w in 0..word_count as u64 {
                  let mut word = input.read_long()? as u64;
                  let base = ((offset_words + w) << 6) as u32;
                  while word != 0 {
                      let bit = word.trailing_zeros();
                      if pos >= count {
                          return Err(corrupt("bitset doc ids overflow count"));
                      }
                      docs[pos] = base.wrapping_add(bit);
                      pos += 1;
                      word &= word - 1;
                  }
              }
              if pos != count {
                  return Err(corrupt(format!(
                      "bitset cardinality {pos} != count {count} (:239)"
                  )));
              }
          }
          16 => {
              // readDelta16 (:242-254): VInt min; count/2 LE ints pairing
              // delta[i] (high 16) with delta[halfLen+i] (low 16); odd tail
              // as one LE short.
              let min = input.read_vint()? as u32;
              let half_len = count / 2;
              let mut packed = Vec::with_capacity(half_len);
              for _ in 0..half_len {
                  packed.push(input.read_int()? as u32);
              }
              for i in 0..half_len {
                  docs[i] = (packed[i] >> 16).wrapping_add(min);
                  docs[half_len + i] = (packed[i] & 0xFFFF).wrapping_add(min);
              }
              if count & 1 == 1 {
                  docs[count - 1] = (input.read_short()? as u16 as u32).wrapping_add(min);
              }
          }
          24 => {
              // readInts24 (:256-274): 8 docs per 3 LE longs, MSB-first
              // 24-bit lanes; tail docs as LE short(doc >>> 8) + byte(doc).
              let mut i = 0usize;
              while i + 8 <= count {
                  let l1 = input.read_long()? as u64;
                  let l2 = input.read_long()? as u64;
                  let l3 = input.read_long()? as u64;
                  docs[i] = (l1 >> 40) as u32;
                  docs[i + 1] = ((l1 >> 16) & 0xFF_FFFF) as u32;
                  docs[i + 2] = (((l1 & 0xFFFF) << 8) | (l2 >> 56)) as u32;
                  docs[i + 3] = ((l2 >> 32) & 0xFF_FFFF) as u32;
                  docs[i + 4] = ((l2 >> 8) & 0xFF_FFFF) as u32;
                  docs[i + 5] = (((l2 & 0xFF) << 16) | (l3 >> 48)) as u32;
                  docs[i + 6] = ((l3 >> 24) & 0xFF_FFFF) as u32;
                  docs[i + 7] = (l3 & 0xFF_FFFF) as u32;
                  i += 8;
              }
              while i < count {
                  docs[i] = ((input.read_short()? as u16 as u32) << 8) | input.read_byte()? as u32;
                  i += 1;
              }
          }
          32 => {
              // readInts32 (:276-278)
              for d in docs.iter_mut() {
                  *d = input.read_int()? as u32;
              }
          }
          other => {
              return Err(corrupt(format!("unknown doc ids flag {other} (:203-205)")));
          }
      }
      Ok(docs)
  }
  ```

  跑测试确认全绿 + fmt + commit：

  ```
  $ cargo test -p codec-lucene9 points_read 2>&1 | tail -3
  test result: ok. 12 passed; 0 failed
  $ cargo fmt --check && echo FMT_OK
  FMT_OK
  $ git add -A && git commit -m "feat: points_read — leaf decode + DocIdsWriter five-branch reverse + 1D intersect three-way prune"
  ```

- [ ] **Step B.5: codec `MaterializedBitmap` 测试（先失败）**

  新建 `crates/codec-lucene9/src/roaring/materialized.rs`，先只写模块文档 + 测试模块（`MaterializedBitmap` 尚不存在 → 编译失败）；同时 `roaring.rs` 接线（`mod frozen;` 一行后）：

  ```rust
  mod materialized;

  pub use materialized::MaterializedBitmap;
  ```

  materialized.rs 本步骤内容：

  ```rust
  //! Owned in-memory `croaring::Bitmap` for materialized query hit sets
  //! (M6 spec §3.3): the points read path materializes per-segment range
  //! hits; croaring types stay inside the codec crate (M5 关键设计事实 7 —
  //! core has no croaring dependency), exposing the same minimal surface
  //! as `super::frozen::FrozenBitmap` (cardinality + docs_from batch reads).
  //! No unsafe: `Bitmap::of` / `iter` are safe croaring APIs.

  #[cfg(test)]
  mod tests {
      use super::*;

      #[test]
      fn batch_iteration_and_seek() {
          let docs: Vec<u32> = (0..10_000u32).map(|i| i * 3).collect();
          let bm = MaterializedBitmap::of(&docs);
          assert_eq!(bm.cardinality(), docs.len() as u64);
          // 从头批量拉全量
          let mut buf = [0u32; 512];
          let mut got: Vec<u32> = Vec::new();
          let mut from = 0;
          loop {
              let n = bm.docs_from(from, &mut buf);
              if n == 0 {
                  break;
              }
              got.extend_from_slice(&buf[..n]);
              from = got.last().unwrap() + 1;
          }
          assert_eq!(got, docs);
          // 落在缝隙上的 seek → 下一个存在的 doc
          let n = bm.docs_from(100, &mut buf);
          assert!(n > 0);
          assert_eq!(buf[0], 102);
          // 越过最大值 → 0
          assert_eq!(bm.docs_from(docs[docs.len() - 1] + 1, &mut buf), 0);
      }

      #[test]
      fn empty_bitmap() {
          let bm = MaterializedBitmap::of(&[]);
          assert_eq!(bm.cardinality(), 0);
          let mut buf = [0u32; 8];
          assert_eq!(bm.docs_from(0, &mut buf), 0);
      }
  }
  ```

  跑测试确认失败：

  ```
  $ cargo test -p codec-lucene9 materialized 2>&1 | tail -3
  error[E0432]: unresolved import `crate::roaring::materialized::MaterializedBitmap`
  ```

- [ ] **Step B.6: `MaterializedBitmap` 实现** — 在 materialized.rs 模块文档后、测试模块前插入：

  ```rust
  /// Owned materialized bitmap over ascending, deduplicated docs.
  pub struct MaterializedBitmap {
      bm: croaring::Bitmap,
  }

  impl MaterializedBitmap {
      /// Bulk-build from ascending, deduplicated docs (`Bitmap::of` fast
      /// path requires sorted input — same contract as `write_term_bitmap`,
      /// roaring.rs:54-58).
      pub fn of(sorted_dedup_docs: &[u32]) -> MaterializedBitmap {
          MaterializedBitmap {
              bm: croaring::Bitmap::of(sorted_dedup_docs),
          }
      }

      pub fn cardinality(&self) -> u64 {
          self.bm.cardinality()
      }

      /// Batch ascending-doc read: fills `dst` with the first docs >=
      /// `from`, returns the count (0 = exhausted). Same semantics as
      /// `FrozenBitmap::docs_from` (frozen.rs:163-168): croaring
      /// `reset_at_or_after` + `next_many` include the current value.
      pub fn docs_from(&self, from: u32, dst: &mut [u32]) -> usize {
          let mut it = self.bm.iter();
          it.reset_at_or_after(from);
          it.next_many(dst)
      }
  }
  ```

  跑测试 + fmt + commit：

  ```
  $ cargo test -p codec-lucene9 materialized 2>&1 | tail -3
  test result: ok. 2 passed; 0 failed
  $ cargo fmt --check && echo FMT_OK
  FMT_OK
  $ git add -A && git commit -m "feat: MaterializedBitmap — owned croaring::Bitmap for materialized point-range hits"
  ```

- [ ] **Step B.7: core 游标泛型化 + `PointsDocIter` + `SegmentDocIter::Points`（先失败测试）**

  先在 `crates/core/src/search/mod.rs` 测试模块追加（编译失败：`PointsDocIter` 尚不存在）：

  ```rust
      /// M6 T-B: PointsDocIter 批量游标（BitmapCursor<MaterializedBitmap>）——
      /// next/advance 序列与物化集合逐点一致（游标形态同 RoaringDocIter，
      /// M5 关键设计事实 5）。
      #[test]
      fn points_doc_iter_cursor_sequences() {
          use codec_lucene9::postings_read::NO_MORE_DOCS;
          use codec_lucene9::roaring::MaterializedBitmap;
          let docs: Vec<u32> = (0..5000u32).map(|i| i * 2).collect();
          let mut it = doc_iter::PointsDocIter::new(MaterializedBitmap::of(&docs));
          assert_eq!(it.next_doc().unwrap(), 0);
          assert_eq!(it.next_doc().unwrap(), 2);
          assert_eq!(it.advance(101).unwrap(), 102); // 落缝 → 下一个
          assert_eq!(it.advance(102).unwrap(), 102); // 已在目标上不动
          assert_eq!(it.doc_id(), 102);
          let mut last = 102;
          loop {
              let d = it.next_doc().unwrap();
              if d == NO_MORE_DOCS {
                  break;
              }
              assert!(d > last);
              last = d;
          }
          assert_eq!(last, 9998);
          assert_eq!(it.next_doc().unwrap(), NO_MORE_DOCS); // 粘滞
      }
  ```

  跑测试确认失败：

  ```
  $ cargo test -p rustlucene-core points_doc_iter 2>&1 | tail -3
  error[E0599]: no associated item named `PointsDocIter` found ...
  ```

- [ ] **Step B.8: doc_iter.rs 泛型化 + PointsDocIter 实现** — 五处编辑：

  ① 头部 import（doc_iter.rs:9）：

  ```rust
  use codec_lucene9::roaring::{FrozenBitmap, MaterializedBitmap};
  ```

  ② `BitmapCursor` 段（doc_iter.rs:518-576）整体替换为 trait + 泛型版本（body 逐字保留，仅 `bitmap` 字段类型参数化）：

  ```rust
  /// Bitmap doc sources for `BitmapCursor` (M5 §2 FrozenBitmap zero-copy
  /// view; M6 §3.3 MaterializedBitmap owned hits materialization). The only
  /// surface the cursor needs.
  pub trait DocsBitmap {
      /// Fills `dst` with the first docs >= `from`, returns the count read
      /// (0 = exhausted) — croaring `reset_at_or_after` + `next_many`.
      fn docs_from(&self, from: u32, dst: &mut [u32]) -> usize;
  }

  impl DocsBitmap for FrozenBitmap {
      fn docs_from(&self, from: u32, dst: &mut [u32]) -> usize {
          FrozenBitmap::docs_from(self, from, dst)
      }
  }

  impl DocsBitmap for MaterializedBitmap {
      fn docs_from(&self, from: u32, dst: &mut [u32]) -> usize {
          MaterializedBitmap::docs_from(self, from, dst)
      }
  }

  /// Batch-refill cursor over a bitmap source (M5 T2, 关键设计事实 5):
  /// the ~60ns frozen-view create / owned-iter create is amortized over a
  /// 512-doc batch; each refill is a `reset_at_or_after` seek + bulk
  /// `next_many`. docs are < max_doc <= i32::MAX, so `d + 1` never
  /// overflows u32.
  pub struct BitmapCursor<B: DocsBitmap> {
      bitmap: B,
      buf: Vec<u32>,
      pos: usize,
      end: usize,
      next_from: u32,
      exhausted: bool,
  }

  const BITMAP_ITER_BATCH: usize = 512;

  impl<B: DocsBitmap> BitmapCursor<B> {
      fn new(bitmap: B) -> BitmapCursor<B> {
          BitmapCursor {
              bitmap,
              buf: vec![0; BITMAP_ITER_BATCH],
              pos: 0,
              end: 0,
              next_from: 0,
              exhausted: false,
          }
      }

      fn refill(&mut self) -> bool {
          if self.exhausted {
              return false;
          }
          self.end = self.bitmap.docs_from(self.next_from, &mut self.buf);
          self.pos = 0;
          if self.end == 0 {
              self.exhausted = true;
              return false;
          }
          true
      }

      fn next(&mut self) -> Option<u32> {
          if self.pos >= self.end && !self.refill() {
              return None;
          }
          let d = self.buf[self.pos];
          self.pos += 1;
          self.next_from = d + 1;
          Some(d)
      }

      /// First doc >= target. Forward-only: discards the buffered tail and
      /// re-seeks (the merge dance's resync pattern, M4 关键设计事实 8).
      fn advance(&mut self, target: u32) -> Option<u32> {
          self.pos = self.end;
          self.next_from = target;
          self.next()
      }
  }
  ```

  ③ 既有使用点类型标注（body 不变）：

  ```rust
  // RoaringDocIter（doc_iter.rs:582-594）字段：
  pub struct RoaringDocIter {
      cur: BitmapCursor<FrozenBitmap>,
      doc: i32,
  }
  // DocSource（doc_iter.rs:628-631）：
  pub enum DocSource {
      Bitmap { cur: BitmapCursor<FrozenBitmap>, doc: Option<u32> },
      Slice { docs: Vec<u32>, pos: usize },
  }
  // DocSource::bitmap(bitmap: FrozenBitmap) —— 签名不变。
  ```

  ④ `RoaringDocIter` 之后追加 `PointsDocIter`：

  ```rust
  // ── Points (M6 §3.3 materialized point-range hits) ───────────────────

  /// DocIter over a PointRange query's materialized per-segment hits
  /// (M6 spec §3.3): same batch-cursor shape as RoaringDocIter. freq() is
  /// 1 — points carry no freqs and `needs_freq` is never routed here.
  pub struct PointsDocIter {
      cur: BitmapCursor<MaterializedBitmap>,
      doc: i32,
  }

  impl PointsDocIter {
      pub fn new(bitmap: MaterializedBitmap) -> PointsDocIter {
          PointsDocIter {
              cur: BitmapCursor::new(bitmap),
              doc: -1,
          }
      }
  }

  impl DocIter for PointsDocIter {
      fn doc_id(&self) -> i32 {
          self.doc
      }

      fn next_doc(&mut self) -> io::Result<i32> {
          if self.doc == NO_MORE_DOCS {
              return Ok(NO_MORE_DOCS);
          }
          self.doc = match self.cur.next() {
              Some(d) => d as i32,
              None => NO_MORE_DOCS,
          };
          Ok(self.doc)
      }

      fn advance(&mut self, target: i32) -> io::Result<i32> {
          if target > self.doc {
              self.doc = match self.cur.advance(target.max(0) as u32) {
                  Some(d) => d as i32,
                  None => NO_MORE_DOCS,
              };
          }
          Ok(self.doc)
      }
  }
  ```

  ⑤ `SegmentDocIter` 枚举（doc_iter.rs:856-867）加变体，三个 match 与 `freq` 各加一臂：

  ```rust
  pub enum SegmentDocIter {
      // ……既有变体不动
      Points(PointsDocIter),
  }
  // doc_id / next_doc / advance 三处 match 各加：
  //     Self::Points(p) => p.doc_id(),   // 其余两处同理（next_doc / advance）
  // freq 的 match 不变（Points 走 `_ => 1` 默认臂）。
  ```

  跑测试（既有全部 + 新游标测试）+ fmt + commit：

  ```
  $ cargo test -p rustlucene-core 2>&1 | tail -2
  test result: ok. ... passed; 0 failed     # 既有 roaring 测试不变（机械泛型化）
  $ cargo test -p rustlucene-core points_doc_iter 2>&1 | tail -2
  test result: ok. 1 passed; 0 failed
  $ cargo fmt --check && echo FMT_OK
  FMT_OK
  $ git add -A && git commit -m "feat: BitmapCursor generic over DocsBitmap + PointsDocIter / SegmentDocIter::Points"
  ```

- [ ] **Step B.9: `Query::PointRange` 接入 search（先失败测试）**

  先在 `crates/core/src/search/mod.rs` 测试模块追加（编译失败：`Query::point_range` 尚不存在）：

  ```rust
      // ── M6 T-B: PointRange ─────────────────────────────────────────

      fn point_schema() -> Schema {
          let mut s = Schema::new();
          s.add(FieldSpec::long_point("ts"));
          s.add(FieldSpec::int_point("lvl"));
          s.add(FieldSpec::keyword("level"));
          s
      }

      /// ts = i*10（LongPoint），lvl = i-50（IntPoint），level 轮转（非 point 字段）
      fn point_doc(i: u32) -> Document {
          let mut d = Document::new();
          d.add("ts", FieldValue::Long(i as i64 * 10));
          d.add("lvl", FieldValue::Int(i as i32 - 50));
          d.add(
              "level",
              FieldValue::Keyword(if i % 2 == 0 { "INFO" } else { "WARN" }.to_string()),
          );
          d
      }

      /// M6 §3.4: 基本语义——count == 物化 cardinality == 迭代数；边界四类。
      #[test]
      fn point_range_query_basic() {
          let root = temp_dir("ptrange");
          let mut w = IndexWriter::create(&root, point_schema(), IndexWriterConfig::default())
              .unwrap();
          for i in 0..100u32 {
              w.add_document(point_doc(i)).unwrap();
          }
          w.commit().unwrap();
          drop(w);
          let dir = FSDirectory::open(&root).unwrap();
          let mut s = Searcher::open(&dir).unwrap();

          // [100, 250] → docs 10..=25
          let q = Query::point_range("ts", 100, 250);
          assert_eq!(s.count(&q).unwrap(), 16);
          let (total, docs) = s.top_docs(&q, 100).unwrap();
          assert_eq!(total, 16);
          assert_eq!(docs, (10..=25).collect::<Vec<i32>>());
          // 迭代器变体钉死物化路径
          let mut reader = Reader::open(&dir).unwrap();
          let (_b, seg) = reader.leaves().next().unwrap();
          let it = q.segment_iterator(seg, false).unwrap().unwrap();
          assert!(
              matches!(it, SegmentDocIter::Points(_)),
              "PointRange must materialize into SegmentDocIter::Points"
          );
          drop(reader);
          // 点查询退化 [v,v]
          assert_eq!(s.count(&Query::point_range("ts", 250, 250)).unwrap(), 1);
          // 不相交 → 0
          assert_eq!(s.count(&Query::point_range("ts", 2000, 3000)).unwrap(), 0);
          // 全区间 MIN..MAX → 100
          assert_eq!(
              s.count(&Query::point_range("ts", i64::MIN, i64::MAX)).unwrap(),
              100
          );
          // 贴 MIN / 贴 MAX
          assert_eq!(s.count(&Query::point_range("ts", i64::MIN, 0)).unwrap(), 1);
          assert_eq!(s.count(&Query::point_range("ts", 990, i64::MAX)).unwrap(), 1);
          // 未知字段 / 非 point 字段 → 空命中（不报错）
          assert_eq!(s.count(&Query::point_range("nope", 0, 1)).unwrap(), 0);
          assert_eq!(s.count(&Query::point_range("level", 0, i64::MAX)).unwrap(), 0);
          // freq_sum 拒绝 PointRange（needs_freq 恒 false，spec §3.3）
          let err = s.freq_sum(&Query::point_range("ts", 0, 1)).unwrap_err();
          assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
          fs::remove_dir_all(&root).unwrap();
      }

      /// M6 §3.1: low>high → Err(InvalidInput)（跨任务钉死接口）。
      /// 注意：Lucene 9.12.3 对 low>high 并不报错（PointRangeQuery.checkArgs
      /// :100-110 仅查 null，自然走成全 Outside → 0 命中）——Err 是本系统
      /// 钉死的显式错误面，不进 Java diff 电池。
      #[test]
      fn point_range_low_gt_high_errors() {
          let root = temp_dir("ptrange-err");
          let mut w = IndexWriter::create(&root, point_schema(), IndexWriterConfig::default())
              .unwrap();
          w.add_document(point_doc(0)).unwrap();
          w.commit().unwrap();
          drop(w);
          let dir = FSDirectory::open(&root).unwrap();
          let mut s = Searcher::open(&dir).unwrap();
          let err = s.count(&Query::point_range("ts", 10, 5)).unwrap_err();
          assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
          let mut c = CountCollector::default();
          let err = s
              .search(&Query::point_range("ts", 10, 5), &mut c)
              .unwrap_err();
          assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
          fs::remove_dir_all(&root).unwrap();
      }

      /// M6 §3.2: 多值点——同 doc 多值逐值回调，物化去重后只计一次。
      #[test]
      fn point_range_multivalued_dedup() {
          let root = temp_dir("ptrange-multi");
          let mut w = IndexWriter::create(&root, point_schema(), IndexWriterConfig::default())
              .unwrap();
          for i in 0..10u32 {
              let mut d = point_doc(i);
              if i == 3 {
                  d.add("ts", FieldValue::Long(10_000));
                  d.add("ts", FieldValue::Long(20_000));
              }
              if i == 7 {
                  d.add("ts", FieldValue::Long(5)); // 与主值 70 同 doc
              }
              w.add_document(d).unwrap();
          }
          w.commit().unwrap();
          drop(w);
          let dir = FSDirectory::open(&root).unwrap();
          let mut s = Searcher::open(&dir).unwrap();
          // [10_000, 20_000] 只命中 doc 3，一次（两个值都命中也只算一次）
          let q = Query::point_range("ts", 10_000, 20_000);
          assert_eq!(s.count(&q).unwrap(), 1);
          let (total, docs) = s.top_docs(&q, 10).unwrap();
          assert_eq!((total, docs), (1, vec![3]));
          // [0, 70] 命中 docs 0..=7（doc 7 两个值都在区间内）→ 8 docs
          assert_eq!(s.count(&Query::point_range("ts", 0, 70)).unwrap(), 8);
          // 全区间 → 仍 10 docs（物化集合去重）
          assert_eq!(
              s.count(&Query::point_range("ts", i64::MIN, i64::MAX)).unwrap(),
              10
          );
          fs::remove_dir_all(&root).unwrap();
      }

      /// M6 §3.1/§3.4: IntPoint 复用同一变体——按 .fnm point_num_bytes
      /// 解包 + clamp 规则（4 字节字段，整区间出界 → 空）。
      #[test]
      fn point_range_int_field_clamp() {
          let root = temp_dir("ptrange-int");
          let mut w = IndexWriter::create(&root, point_schema(), IndexWriterConfig::default())
              .unwrap();
          for i in 0..100u32 {
              w.add_document(point_doc(i)).unwrap(); // lvl = i-50 ∈ [-50, 49]
          }
          w.commit().unwrap();
          drop(w);
          let dir = FSDirectory::open(&root).unwrap();
          let mut s = Searcher::open(&dir).unwrap();
          // i64 全域 → clamp 到 i32 全域 → 100
          assert_eq!(
              s.count(&Query::point_range("lvl", i64::MIN, i64::MAX)).unwrap(),
              100
          );
          // 部分出界 clamp：[i64::MIN, -40] → lvl ∈ [-50, -40] → docs 0..=10
          assert_eq!(s.count(&Query::point_range("lvl", i64::MIN, -40)).unwrap(), 11);
          // 整区间出 i32 域 → 0（clamp-to-empty，非错误）
          assert_eq!(
              s.count(&Query::point_range("lvl", i32::MAX as i64 + 1, i64::MAX))
                  .unwrap(),
              0
          );
          assert_eq!(
              s.count(&Query::point_range("lvl", i64::MIN, i32::MIN as i64 - 1))
                  .unwrap(),
              0
          );
          // 负值边界
          assert_eq!(s.count(&Query::point_range("lvl", -50, -50)).unwrap(), 1);
          fs::remove_dir_all(&root).unwrap();
      }

      /// M6 §3.3: 多段 doc base 映射 + 段级空结果。
      #[test]
      fn point_range_multi_segment() {
          let root = temp_dir("ptrange-seg");
          let mut w = IndexWriter::create(&root, point_schema(), IndexWriterConfig::default())
              .unwrap();
          for i in 0..10u32 {
              w.add_document(point_doc(i)).unwrap();
          }
          w.commit().unwrap();
          for i in 10..25u32 {
              w.add_document(point_doc(i)).unwrap();
          }
          w.commit().unwrap();
          drop(w);
          let dir = FSDirectory::open(&root).unwrap();
          let mut s = Searcher::open(&dir).unwrap();
          // ts = i*10; [50, 200] → docs 5..=20
          let q = Query::point_range("ts", 50, 200);
          assert_eq!(s.count(&q).unwrap(), 16);
          let (total, docs) = s.top_docs(&q, 100).unwrap();
          assert_eq!(total, 16);
          assert_eq!(docs, (5..=20).collect::<Vec<i32>>());
          // 只命中第二段
          let (total, docs) = s
              .top_docs(&Query::point_range("ts", 150, 240), 100)
              .unwrap();
          assert_eq!(total, 10);
          assert_eq!(docs, (15..=24).collect::<Vec<i32>>());
          fs::remove_dir_all(&root).unwrap();
      }
  ```

  跑测试确认失败：

  ```
  $ cargo test -p rustlucene-core point_range 2>&1 | tail -3
  error[E0599]: no function or associated item named `point_range` found for enum `Query`
  ```

- [ ] **Step B.10: query.rs / segment_reader.rs / searcher.rs 接线实现** — 六处编辑：

  ① `query.rs` 枚举（:14-24）加变体 + 构造器（`Query::phrase` 构造器之后）：

  ```rust
  pub enum Query {
      // ……既有变体不动
      /// 1D point range (M6 spec §3.1), LongPoint/IntPoint `newRangeQuery`
      /// semantics: both ends inclusive; `low > high` is rejected with
      /// `Err(InvalidInput)` at execution (跨任务钉死接口).
      PointRange { field: String, low: i64, high: i64 },
  }

      /// 1D point range query (M6 §3.1): inclusive both ends. 排他边界由
      /// 调用方 ±1 调整（避免 MIN/MAX 溢出特例）。
      pub fn point_range(field: &str, low: i64, high: i64) -> Query {
          Query::PointRange {
              field: field.to_string(),
              low,
              high,
          }
      }
  ```

  ② `query.rs` 头部 import 补 `PointsDocIter` 与 `MaterializedBitmap`：

  ```rust
  use codec_lucene9::roaring::MaterializedBitmap;

  use super::doc_iter::{
      ConjunctionDocIter, DisjunctionDocIter, MatchAllIter, PhraseDocIter, PointsDocIter,
      RoaringDocIter, SegmentDocIter,
  };
  ```

  ③ `Query::segment_iterator`（:126-196）加一臂（`Query::Phrase` 臂之后）：

  ```rust
              Query::PointRange { field, low, high } => {
                  let Some(bm) = point_range_bitmap(seg, field, *low, *high)? else {
                      return Ok(None);
                  };
                  Ok(Some(SegmentDocIter::Points(PointsDocIter::new(bm))))
              }
  ```

  ④ `query.rs` 文件尾部追加共享物化入口：

  ```rust
  /// PointRange 共享物化入口（`segment_iterator` 与 `Searcher::count` 同一
  /// 物化，spec §3.3）：`low > high` → `Err(InvalidInput)`（跨任务钉死接口；
  /// Lucene 9.12.3 对该情形不报错而返回 0 命中，PointRangeQuery.checkArgs
  /// :100-110——此处是钉死的 Rust 显式错误面）。段内命中经 visitor 收集、
  /// sort + dedup 后建 `MaterializedBitmap`；`None` = 未知字段 / 非 point
  /// 字段 / 段无 points / 空命中。
  pub(crate) fn point_range_bitmap(
      seg: &mut SegmentReader,
      field: &str,
      low: i64,
      high: i64,
  ) -> io::Result<Option<MaterializedBitmap>> {
      if low > high {
          return Err(io::Error::new(
              io::ErrorKind::InvalidInput,
              format!("point range low ({low}) > high ({high})"),
          ));
      }
      let Some(points) = seg.points_reader() else {
          return Ok(None);
      };
      let mut docs: Vec<u32> = Vec::new();
      // 多值点逐值回调（spec §3.2）；BKD 访问序按 (value, doc) 非 doc 序，
      // 统一 sort + dedup 再建 bitmap（Bitmap::of 快路径要求升序，去重同时
      // 解决多值点重复计数）。
      points.intersect(field, low, high, &mut |_value, doc| {
          docs.push(doc as u32)
      })?;
      docs.sort_unstable();
      docs.dedup();
      if docs.is_empty() {
          return Ok(None);
      }
      Ok(Some(MaterializedBitmap::of(&docs)))
  }
  ```

  ⑤ `segment_reader.rs`：import + 字段 + open 挂载 + accessor：

  ```rust
  use codec_lucene9::points_read::PointsReader;
  // struct SegmentReader 加字段：
  //     points: Option<PointsReader>,
  // open 中（postings 之后；`segment`/`segment_id` 即现有局部变量
  // `&sci.info.name` / `&sci.info.id`，segment_reader.rs:23-24）：
  //     let points = PointsReader::open(dir, segment, segment_id, &field_infos)?;
  // impl 块加：
  /// Per-segment points reader (M6 §3.2); `None` when the segment has no
  /// points files (segment_builder.rs:240-245).
  pub(crate) fn points_reader(&self) -> Option<&PointsReader> {
      self.points.as_ref()
  }
  ```

  ⑥ `searcher.rs`：import 行（:12）`use super::query::Query;` → `use super::query::{self, Query};`；`count` 的 And/Or 臂之前插入：

  ```rust
          // M6 §3.3: PointRange count = 物化 bitmap cardinality 直读（与迭代
          // 同一物化；Lucene PointRangeQuery 对 count 同样是 visitor 全量收集）。
          if let Query::PointRange { field, low, high } = query {
              let mut total = 0u64;
              for (_doc_base, seg) in self.reader.leaves() {
                  if let Some(bm) = query::point_range_bitmap(seg, field, *low, *high)? {
                      total += bm.cardinality();
                  }
              }
              return Ok(total);
          }
  ```

  `freq_sum` 拒绝集合加 PointRange（:138-144）：

  ```rust
          if query.is_multi_term()
              || matches!(query, Query::And { .. } | Query::Or { .. } | Query::PointRange { .. })
          {
              return Err(io::Error::new(
                  io::ErrorKind::InvalidInput,
                  "freq_sum is only defined for Term queries (MatchAll degenerates to \
                   doc count); multi-term, And/Or and PointRange queries have no \
                   well-defined freq sum",
              ));
          }
  ```

  跑测试 + fmt + commit：

  ```
  $ cargo test -p rustlucene-core point_range 2>&1 | tail -3
  test result: ok. 5 passed; 0 failed
  $ cargo test -p rustlucene-core 2>&1 | tail -2
  test result: ok. ... passed; 0 failed
  $ cargo test -p codec-lucene9 2>&1 | tail -2
  test result: ok. ... passed; 0 failed
  $ cargo fmt --check && echo FMT_OK
  FMT_OK
  $ git add -A && git commit -m "feat: Query::PointRange — BKD intersect materialization into search (count = bitmap cardinality)"
  ```

- [ ] **Step B.11: rustlucene-cli——searchbench RANGE 行解析**

  **RANGE 行语法**（查询文件 = Java `SearchBench --dump-queries` 产物，全部行型 tab 分隔，RANGE 同式）：

  ```
  RANGE\t<field>\t<low>\t<high>
  ```

  - `field`：point 字段名（独立于 searchbench 位置参数 `<field>`——term 字段与 point 字段不同名是常态，log schema 即 `message` vs `timestamp`）。
  - `low`/`high`：i64 十进制（可负；IntPoint 字段的 clamp 规则与查询语义一致，两侧各自实现）。
  - bucket 列固定伪桶 `all`：label = `range\tall`；per-query 计数 detail 行 =
    `range field=<f> low=<low> high=<high> bucket=range\tall\t<count>`（与既有 detail 行同形，stderr 输出，sort 后两侧 diff）。
  - 示例行（log 语料 `TS_BASE = 1_700_000_000_000`，200000 docs）：

    ```
    RANGE	timestamp	1699999900000	1700000010000
    RANGE	timestamp	1700100000000	1700200000000
    ```

  ① `rustlucene-cli.rs` searchbench 查询文件解析（`phrase_tasks` 解析块之后，:458-473 区域）追加：

  ```rust
      // M6 RANGE lines (same file, replayed verbatim like AND/OR/PHRASE):
      // RANGE\t<field>\t<low>\t<high>；low>high 行会让查询在执行期
      // Err(InvalidInput)（跨任务钉死接口），dump 侧不写这种行。
      let range_tasks: Vec<(String, i64, i64)> = content
          .lines()
          .filter(|l| l.starts_with("RANGE\t"))
          .filter_map(|l| {
              let parts: Vec<&str> = l.split('\t').collect();
              if parts.len() >= 4 {
                  Some((
                      parts[1].to_string(),
                      parts[2].parse::<i64>().unwrap_or(0),
                      parts[3].parse::<i64>().unwrap_or(0),
                  ))
              } else {
                  None
              }
          })
          .collect();
  ```

  ② `WorkItem` 枚举（:503-512）加变体：

  ```rust
          Range(String, i64, i64),
  ```

  ③ work 列表构建（`phrase_tasks` 推送块之后，:559-564 区域）：

  ```rust
      // RANGE lines: replayed verbatim, appended after the M2 line types.
      for (f, low, high) in &range_tasks {
          work.push(("range\tall".to_string(), WorkItem::Range(f.clone(), *low, *high)));
      }
  ```

  ④ `build_query`（:571-584）加一臂：

  ```rust
              WorkItem::Range(f, low, high) => Query::point_range(f, *low, *high),
  ```

  ⑤ `detail_of`（:601-612）加一臂：

  ```rust
              WorkItem::Range(f, low, high) => {
                  format!("range field={f} low={low} high={high} bucket={label}")
              }
  ```

  编译 + 单测回归 + fmt + commit：

  ```
  $ cargo test -p rustlucene-core 2>&1 | tail -2
  test result: ok. ... passed; 0 failed
  $ cargo fmt --check && echo FMT_OK
  FMT_OK
  $ git add -A && git commit -m "feat: searchbench RANGE line parsing (rust side)"
  ```

- [ ] **Step B.12: rustlucene-cli——searchdump range 电池行**

  ① searchdump range 电池行（matchall first20 输出之后、`// Boolean battery` 注释之前，:197-199 区域插入——插入位置必须与 VerifySearchIndex.java 的插入位置同相对序）：

  ```rust
      // M6 §3.4 range battery (timestamp = LongPoint)：边界确定性推导——
      // ts(doc i) ∈ [TS_BASE + i*1000, TS_BASE + i*1000 + 999]
      // (gen_log_document :132)，所以 [base+100000, base+199999] 恰好命中
      // docs 100..=199。逐行镜像在 VerifySearchIndex.java。
      if num_docs >= 200 {
          let range_battery: [(i64, i64); 5] = [
              (TS_BASE + 100_000, TS_BASE + 199_999), // docs 100..=199 → 100
              (TS_BASE - 1_000_000, TS_BASE - 1),     // 不相交 → 0
              (i64::MIN, i64::MAX),                   // 全区间 → num_docs
              (i64::MIN, TS_BASE + 49_999),           // 贴 MIN → docs 0..=49 → 50
              (TS_BASE + 150_000, i64::MAX),          // 贴 MAX → docs 150.. → num_docs-150
          ];
          for (low, high) in range_battery {
              let q = Query::point_range("timestamp", low, high);
              let count = searcher.count(&q)?;
              let (_, docs) = searcher.top_docs(&q, 20)?;
              out.push_str(&format!(
                  "range timestamp=[{low},{high}] count={count} first20={}\n",
                  doc_csv(&docs)
              ));
          }
          // 点查询退化 [v,v] 恰好命中 doc 100
          let q = Query::point_range("timestamp", TS_BASE + 100_000, TS_BASE + 100_999);
          let count = searcher.count(&q)?;
          out.push_str(&format!(
              "range timestamp=[{},{}] count={count}\n",
              TS_BASE + 100_000,
              TS_BASE + 100_999
          ));
      }
  ```

  编译 + 单测回归 + fmt + commit：

  ```
  $ cargo test -p rustlucene-core 2>&1 | tail -2
  test result: ok. ... passed; 0 failed
  $ cargo fmt --check && echo FMT_OK
  FMT_OK
  $ git add -A && git commit -m "feat: searchdump range battery rows (log timestamp LongPoint)"
  ```

- [ ] **Step B.13: Java 两侧——SearchBench RANGE 行解析 + VerifySearchIndex range 电池行**

  ① `interop/java/SearchBench.java`：import 段（:5）补：

  ```java
  import org.apache.lucene.document.IntPoint;
  import org.apache.lucene.document.LongPoint;
  ```

  ② `--load-queries` 解析（:415 `List<String[]> loadedPhrase` 声明后加 `List<String[]> loadedRange = new ArrayList<>();`；:430-431 的 PHRASE 分支后加）：

  ```java
                      } else if (parts[0].equals("RANGE") && parts.length >= 4) {
                          loadedRange.add(new String[]{parts[1], parts[2], parts[3]});
                      }
  ```

  ③ 查询构建（:524-528 `loadedPhrase` 循环之后）：

  ```java
                  // M6 RANGE lines: LongPoint/IntPoint newRangeQuery chosen by
                  // the field's point width (IntPoint clamp mirrors the Rust
                  // codec rule; a range fully outside the int domain matches
                  // nothing on both sides).
                  for (String[] p : loadedRange) {
                      String f = p[0];
                      long low = Long.parseLong(p[1]), high = Long.parseLong(p[2]);
                      FieldInfo fi = FieldInfos.getMergedFieldInfos(reader).fieldInfo(f);
                      Query q;
                      if (fi != null && fi.getPointNumBytes() == Integer.BYTES) {
                          if (low > Integer.MAX_VALUE || high < Integer.MIN_VALUE) {
                              q = new MatchNoDocsQuery("range fully outside int domain");
                          } else {
                              int lo = (int) Math.max(low, (long) Integer.MIN_VALUE);
                              int hi = (int) Math.min(high, (long) Integer.MAX_VALUE);
                              q = IntPoint.newRangeQuery(f, lo, hi);
                          }
                      } else {
                          q = LongPoint.newRangeQuery(f, low, high);
                      }
                      queries.add(q);
                      labels.add("range\tall");
                      details.add("range field=" + f + " low=" + low + " high=" + high
                              + " bucket=range\tall");
                  }
  ```

  ④ `interop/java/VerifySearchIndex.java`：matchall first20 块（:69-76）之后、Boolean 电池之前插入（与 rust searchdump 同相对序）：

  ```java
              // M6 §3.4 range battery — 与 rustlucene-cli searchdump 逐行镜像；
              // ts(doc i) ∈ [TS_BASE + i*1000, TS_BASE + i*1000 + 999]
              if (r.maxDoc() >= 200) {
                  long TS = 1_700_000_000_000L;
                  long[][] ranges = {
                      {TS + 100_000, TS + 199_999},   // docs 100..=199 → 100
                      {TS - 1_000_000, TS - 1},       // 不相交 → 0
                      {Long.MIN_VALUE, Long.MAX_VALUE}, // 全区间 → maxDoc
                      {Long.MIN_VALUE, TS + 49_999},  // 贴 MIN → docs 0..=49 → 50
                      {TS + 150_000, Long.MAX_VALUE}, // 贴 MAX
                  };
                  for (long[] rg : ranges) {
                      Query q = LongPoint.newRangeQuery("timestamp", rg[0], rg[1]);
                      TopDocs td = s.search(q, 20, Sort.INDEXORDER);
                      StringBuilder b = new StringBuilder();
                      for (ScoreDoc sd : td.scoreDocs) b.append(sd.doc).append(',');
                      out.append("range timestamp=[").append(rg[0]).append(',').append(rg[1])
                         .append("] count=").append(s.count(q))
                         .append(" first20=").append(b).append('\n');
                  }
                  Query q = LongPoint.newRangeQuery("timestamp", TS + 100_000, TS + 100_999);
                  out.append("range timestamp=[").append(TS + 100_000).append(',')
                     .append(TS + 100_999).append("] count=").append(s.count(q)).append('\n');
              }
  ```

  编译 + commit（Java 行为验证在 Step B.14 的电池里）：

  ```
  $ make java-classes
  javac -cp "..." -d interop/java/classes interop/java/*.java   # 零错误零警告
  $ git add -A && git commit -m "feat: SearchBench/VerifySearchIndex RANGE lines — LongPoint/IntPoint newRangeQuery dual-side parsing"
  ```

- [ ] **Step B.14: 验收（spec §3.4 全覆盖）** — 本步骤不改代码，无 commit：

  ① 单测全绿：

  ```
  $ cargo test -p codec-lucene9 2>&1 | tail -2
  test result: ok. ... passed; 0 failed
  $ cargo test -p rustlucene-core 2>&1 | tail -2
  test result: ok. ... passed; 0 failed
  ```

  ② `make log-test` 五变体（seed 42 默认 / 43 `--positions` / 44 `--sparse` / 45 `--bigdict` / 46 `--bitmap`）——searchdump vs VerifySearchIndex 的 diff 现在包含 6 行 range 行（Step B.12①/B.13④），逐条一致：

  ```
  $ make log-test 2>&1 | grep -c '^range timestamp='
  30    # 5 变体 × 6 行（cat /tmp/rl-search-rust.out 各打印一遍）
  $ make log-test 2>&1 | grep -E 'LOG_INTEROP_OK' | wc -l
  5
  ```

  ③ searchbench RANGE 行 Java 逐条 diff（spec §3.4 头条）：用 log-test 产物索引（或现场重建），Java dump 查询文件后手工追加 RANGE 行，两侧 `--load-queries` 回放，stderr 计数行 sort 后 diff 为空：

  ```
  $ CP="interop/java/classes:interop/java/lib/lucene-core-9.12.3.jar:interop/java/lib/lucene-analysis-common-9.12.3.jar"
  $ cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- logwrite /tmp/m6b-rust 200000 42
  $ java -cp "$CP" JavaLogBench /tmp/m6b-java 200000 1 42
  $ java -cp "$CP" SearchBench /tmp/m6b-java message --dump-queries /tmp/m6b-queries.txt --tasks 5 --seed 42
  $ printf 'RANGE\ttimestamp\t1699999900000\t1700000009999\nRANGE\ttimestamp\t1700100000000\t1700200000000\nRANGE\ttimestamp\t-9223372036854775808\t1700000049999\n' >> /tmp/m6b-queries.txt
  $ cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
      searchbench /tmp/m6b-rust message --load-queries /tmp/m6b-queries.txt --warmup 1 --iter 1 \
      2> /tmp/m6b-rust-counts.txt > /dev/null
  $ java -cp "$CP" SearchBench /tmp/m6b-java message --load-queries /tmp/m6b-queries.txt \
      --warmup 1 --iter 1 --no-cache 2> /tmp/m6b-java-counts.txt > /dev/null
  $ grep '^range ' /tmp/m6b-rust-counts.txt | sort > /tmp/m6b-r.txt
  $ grep '^range ' /tmp/m6b-java-counts.txt | sort > /tmp/m6b-j.txt
  $ diff /tmp/m6b-r.txt /tmp/m6b-j.txt && echo RANGE_DIFF_OK
  RANGE_DIFF_OK
  $ cat /tmp/m6b-r.txt
  range field=timestamp low=-9223372036854775808 high=1700000049999 bucket=range	all	50
  range field=timestamp low=1699999900000 high=1700000009999 bucket=range	all	10
  range field=timestamp low=1700100000000 high=1700200000000 bucket=range	all	99900
  ```

  （锚点推导：ts(doc i) ∈ [base+i·1000, base+i·1000+999]，base=1700000000000。
  第三行 [base+100e6, base+200e6] 覆盖 docs 100000..199999 = 99900；第一行贴
  MIN、[MIN, base+49999] 覆盖 docs 0..49 = 50；第二行 [base−100000, base+9999]
  覆盖 docs 0..9 = 10（i=10 的最小 ts = base+10000 > high，边界行列不留 rng
  余地）。若 diff 非空，说明物化/去重/边界有错，按 systematic-debugging 回查
  Step B.4/B.10。）

  ④ 边界四类核对清单（spec §3.4）逐一打勾：

  - 不相交区间 → 命中 0：codec `intersect_disjoint_is_empty`、core `point_range_query_basic`（2000..3000）、电池行 `range timestamp=[1699999000000,1699999999999] count=0` ✓
  - `low>high` → `Err(InvalidInput)`：core `point_range_low_gt_high_errors`（count + search 两路）✓（**不进 Java diff**——Java 不报错返回 0，见设计要点 4 的事实修正）
  - 全区间 MIN..MAX：codec/core 单测 + 电池行 `count=200000` ✓
  - 单边贴 MIN / 贴 MAX：codec `intersect_boundaries_min_max` + 电池行 `[MIN, base+49999] count=50` / `[base+150000, MAX] count=199850` ✓
  - IntPoint 字段：codec `intersect_int_field_clamps_bounds` + core `point_range_int_field_clamp`（clamp/整域出界/负值边界）✓
  - 多值点去重：codec `intersect_multivalued_docs_callback_per_value`（逐值回调不去重）+ core `point_range_multivalued_dedup`（物化去重计一次）✓

  ⑤ 记账：`.superpowers/sdd/progress.md` 追加 T-B 完成记录（review 结论、设计要点 4 的 spec 事实修正、deferred Minor）。

---

### Task C: forceMerge(1) 段归并

格式级归并：读当前 `segments_N` 指向的全部段 → 逐格式归并出一个新段 → 新 `.si` → 复用
`index_writer.rs` 的两段式 `segments_N` 提交 → 成功后删旧段文件与旧 `segments_N`。单线程。
对照 `reference/lucene-9.12.3/` 的 `SegmentMerger.merge()`（SegmentMerger.java:113）与
`IndexWriter.forceMerge`（merge diagnostics：IndexWriter.java:5014-5016 + setDiagnostics
:5029-5044）。**必须在 T-B 完成后开工**（points 归并消费 `points_read::PointsReader`）。

**Files:**

- Create: `crates/core/src/merge.rs`（`force_merge` 总装 + 逐格式归并 + 纯函数 `merge_sorted_dicts`/`build_ord_remap`/`assert_field_infos_consistent` + 全部 core 侧测试）
- Create: `crates/codec-lucene9/src/doc_values_read.rs`（`.dvm/.dvd` 顺序读：NumericDV 值/DISI docs、SortedDV 字典 + 逐 doc ord）
- Modify: `crates/codec-lucene9/src/stored_fields.rs`（新增 `StoredFieldsIndexReader`（.fdx/.fdm 块索引读）+ `StoredFieldsWriter::append_raw_chunk`（裸 chunk 追加，docBase 重定基）+ 测试）
- Modify: `crates/codec-lucene9/src/lib.rs`（`pub mod doc_values_read;`）
- Modify: `crates/core/src/lib.rs`（`pub mod merge;`）
- Modify: `crates/core/src/index_writer.rs`（`commit_infos` 私有 → `pub(crate)`，merge 复用）
- Modify: `crates/codec-lucene9/src/field_infos.rs`（`FieldInfo` 加 `#[derive(Clone, PartialEq, Eq, Debug)]`——逐字段一致性断言用；现存枚举 `IndexOptions`/`DocValuesType`/`VectorEncoding`/`VectorSimilarity` 已 derive PartialEq/Eq，字段类型（String/bool/i32/i64/BTreeMap）均支持，无连锁改动）
- Modify: `crates/core/src/bin/rustlucene-cli.rs`（`forcemerge` 子命令；`logwrite` 增加 `--flush-every N`——电池变体要"多段 + 语料与 searchdump 重放一致"的索引，见关键代码事实 12）
- Modify: `interop/verify-log.sh`（正文包进 `run_standard_variant`，新增 `--forcemerge` / `--forcemerge-bitmap` 分发）
- Modify: `Makefile`（log-test 五变体 → 七变体）
- Test: 上述各文件的 `#[cfg(test)]` 模块（项目无 tests/ 目录，测试一律内联）

**Interfaces:**

- Consumes:
  ```rust
  // T-B 交付（钉死签名，Global Constraints）：
  codec_lucene9::points_read::PointsReader
  PointsReader::open(dir: &FSDirectory, segment: &str, segment_id: &[u8; 16], field_infos: &FieldInfos)
      -> io::Result<Option<PointsReader>>
  PointsReader::intersect(&self, field: &str, low: i64, high: i64,
      visitor: &mut dyn FnMut(i64, i32)) -> io::Result<()>
  // 既有读写路径（全部已存在，签名以源码为准）：
  SegmentInfos::read_latest(dir) -> io::Result<(SegmentInfos, i64)>     // segment_infos.rs:307
  SegmentInfos::commit(&self, dir, generation) -> io::Result<()>        // segment_infos.rs:203（两段式+fsync）
  FieldInfos::read/write(dir, segment, segment_id, suffix)              // field_infos.rs:147/186
  TermsDict::open(dir, segment, segment_id, field_infos)                // terms_read.rs:89
  TermsDict::terms_iter(&mut self, field) -> TermsIter                  // terms_read.rs:417；next() -> Option<(Vec<u8>, TermEntry)>，词典升序
  PostingsReader::{docs, docs_and_freqs, positions}(&self, &TermEntry)  // postings_read.rs:95/103/121
  PositionsEnum::{next_doc, freq, next_position}                        // postings_read.rs:750/758/764
  PostingsWriter::new(dir, segment, segment_id)?.with_bitmap_threshold(Option<u32>) // postings.rs:224/303
  PostingsWriter::{start_field, write_term, finish_field, finish}       // postings.rs:308/349/393/445
  StoredFieldsWriter::new(dir, segment, seg_id, suffix)? / .finish(max_doc, dir)     // stored_fields.rs:328/482
  DocValuesWriter::{add_numeric_field, add_sorted_field, finish}        // doc_values.rs:111/129/162
  PointsWriter::{write_field_long, write_field_int, finish}             // points.rs:131/147/236
  SegmentInfo::new(name, id, doc_count) / .write(dir, "")               // segment_info.rs:50/70
  segment_infos::{random_id, SEGMENTS}                                        // segment_infos.rs:330/20
  packed::{DirectReader, DirectMonotonicReader}                         // packed.rs:171/217（读方向已存在！）
  codec_util::{check_index_header, check_footer}                        // 读侧 header/footer 校验
  index_writer::commit_infos(dir, &mut infos, generation)               // index_writer.rs:144（本任务改 pub(crate)）
  segment_builder::to_base36                                            // segment_builder.rs:305（新段名）
  IndexWriterConfig { bitmap: bool, bitmap_threshold: u32, .. }         // index_writer.rs:11-26
  ```
- Produces（T-C 交付物，签名冻结）:
  ```rust
  // crates/core/src/merge.rs（Global Constraints 钉死）
  pub fn force_merge(dir: &FSDirectory, config: &IndexWriterConfig) -> io::Result<()>
  // crates/codec-lucene9/src/stored_fields.rs（本任务新增）
  pub struct StoredFieldsIndexReader { /* .fdm 元数据 + 两个 DirectMonotonicReader */ }
  impl StoredFieldsIndexReader {
      pub fn open(dir: &FSDirectory, segment: &str, segment_id: &[u8; 16]) -> io::Result<Self>;
      pub fn num_chunks(&self) -> usize;
      pub fn chunk_doc_count(&self, chunk: usize) -> i32;   // docsDM[chunk+1] - docsDM[chunk]
      pub fn chunk_byte_range(&self, chunk: usize) -> (u64, u64); // .fdt [startFp, endFp)
  }
  impl StoredFieldsWriter {
      /// 裸 chunk 追加（Lucene90CompressingStoredFieldsWriter.copyChunks :520-595 的写侧）：
      /// 重写 docBase 为当前 doc_base 后原样写 code 与 payload；不得与 write_field 混用。
      pub fn append_raw_chunk(&mut self, num_docs: i32, code: i32, payload: &[u8]) -> io::Result<()>;
  }
  // crates/codec-lucene9/src/doc_values_read.rs（本任务新增）
  pub struct DocValuesReader { /* .dvm 条目 + .dvd 字节 */ }
  impl DocValuesReader {
      pub fn open(dir: &FSDirectory, segment: &str, segment_id: &[u8; 16],
                  suffix: &str) -> io::Result<Self>;
      pub fn numeric_values(&self, field_number: i32) -> io::Result<Vec<(u32, i64)>>; // (doc, value)，doc 升序
      pub fn sorted_dict(&self, field_number: i32) -> io::Result<Vec<Vec<u8>>>;       // 字典序
      pub fn sorted_ords(&self, field_number: i32) -> io::Result<Vec<(u32, u32)>>;    // (doc, ord)，doc 升序
  }
  ```

**关键代码事实（本计划全部步骤的依据，已逐项对照本仓库源码核实；与 spec §4 假设不符处单列）：**

1. **`packed.rs` 读方向已存在**：`DirectReader`（packed.rs:171）与 `DirectMonotonicReader`
   （packed.rs:217，含 round-trip 测试）已落地。spec §4.3 说 "`.fdx` 块索引读（DirectMonotonic
   读方向）" 需新建——实际需要新建的只是把它接到 `.fdx/.fdm` 布局上的
   `StoredFieldsIndexReader`，DirectMonotonic 本身零新代码。
2. **stored 裸拷贝不是 100% 字节原样**：chunk 头两个 VInt（docBase、code）必须重写——
   docBase 重定基为新段 doc 序，code（numDocs<<2|dirty|sliced）原样；其余字节（numStoredFields、
   lengths、LZ4 压缩数据）逐字节拷贝（Java copyChunks :552-595 同款）。本系统无 delete、chunk
   从不跨段 ⇒ 恒走 copyChunks 主路径，`copyOneDoc` 文档级回退不存在（前提与 Java
   `mergeSub.canBulkCopy` 相同：同 codec、无 liveDocs、同 chunkSize）。
3. **postings 归并免交错**：新段 = 各段按 `segments_N` 顺序顺序拼接，`doc_new = doc_base +
   doc_old`（doc_base = 前序段 maxDoc 累加）。同一 term 的合并文档流 = 各段 postings 顺序
   拼接 + 偏移，天然严格升序——k-way 归并只发生在**词典序**上（各段 TermsIter 已升序），
   文档流无需任何交错/去重。positions 按 doc 序原样拼接（pos 无需偏移）。
4. **singleton/df==1**：`PostingsWriter.write_term` 对 df==1 自动走 singleton 路径（postings.rs:376，
   .doc 不落字节）；读侧 `TermEntry.state.singleton_doc_id` 由枚举自动展开
   （postings_read.rs:546-551）。归并代码零特例。
5. **bitmap 重建零新代码**：`PostingsWriter.with_bitmap_threshold(Some(t))` 后 df ≥ t 自动内联
   重建（postings.rs:368-372 的 hook，写于 docStartFP 捕获之前）；归并 df 已变，旧 bitmap 字节
   不读不复用。读侧门槛 `roaring::BITMAP_MIN_DF = 4096`，CLI 与 logwrite 一样 clamp 到 ≥4096。
6. **merged `.fnm` 直接复用段 0**：各段 `.fnm` 逐字段断言一致后，以新段名/新 seg id 重写——
   FieldInfo 内容（含 `doc_values_gen = -1`，segment_builder.rs:130）原样。Java merge 重建
   FieldInfos；本系统同源写出，不变量等价。
7. **merged `.si` 的 diagnostics 只写稳定键**：`source=merge`、`lucene.version=9.12.3`、
   `mergeFactor=<段数>`（对齐 IndexWriter.java:5014-5016/5031-5032）；Java 还写
   os/java.vendor/timestamp 等非确定键，本系统 flush 段也只写两个稳定键
   （segment_builder.rs:269-272），merge 同款——逐字节 diff 两种 Java 产物本来就不比较
   diagnostics（VerifyLogIndex 不 dump 它）。attributes 必须带
   `Lucene90StoredFieldsFormat.mode=BEST_SPEED`（读侧必需，segment_info.rs:25-26）。
8. **新段命名**：`_` + base36(`SegmentInfos.counter`)（counter 来自 `segments_N`，即下一段号），
   seg id = `random_id()`，SCI id = `random_id()`；commit 后 `infos.counter` 递增。
   maxDoc = 各段 doc_count 之和。
9. **写入顺序必须 stored → postings → DV → points**：stored 裸拷贝在 `.fdt` 上追加 chunk，
   postings 若先失败会留下已部分写入的 `.fdt`；先 stored 则 postings 失败时 stored 已完整，
   孤儿文件清单清理逻辑统一（反正都要删）。格式间无依赖，顺序仅为失败清理简单。
10. **0-doc 段在本系统不存在**：`SegmentBuilder::finalize` 对空 buffer 返回 `None`
    （segment_builder.rs:96-98），不可能提交空段。spec §4.4 的"空段（0 docs）"边界改写为
    两个等价覆盖：① 全索引无段（`segments_N` 里 0 段）→ `force_merge` no-op 报错或无操作
    （计划选 `Ok(())` no-op）；② 字段级空（某段全稀疏 DV / 无 points / 无 terms 的字段），
    由 `field_has_terms`/`field_has_points` 同类判定在归并侧覆盖。
11. **`.fnm` 与 `.si` 的 seg id**：`.fnm`/`.fdt`/`.dvd` 等所有 per-segment 文件的 index header
    都嵌段 id——merged 段必须用**同一个新 random_id** 写全部文件，否则 Java 读侧
    header 校验失败（各 reader `check_index_header` 逐一比对）。
12. **logwrite 单写线程默认只产 1 段**（`max_buffered_docs = 1_000_000` > 电池 200K docs）。
    多段索引两条现存路：logbench 8 线程（但每线程独立 seed 流 + doc_id 归零重开，
    语料≠单写线程重放）或多次 commit。**spec §4.4③ 的交叉 diff 逼出选择**：Java
    VerifySearchIndex 的 trace_id/phrase 电池行从 stored 取真值
    （VerifySearchIndex.java:60-65/151-153），Rust searchdump 从 seed 重放
    （rustlucene-cli.rs:734-768）——只有索引语料 == 单写线程重放语料时两侧行内容
    才逐字节一致。logbench 产物不满足 ⇒ 电池多段索引改用 **logwrite + 新
    `--flush-every N`**（IndexWriter.add_document 既有 flush 触发器，index_writer.rs:95-99，
    8 段一个 commit）；语料与重放一致，③ 才能做全文件 diff。与 spec 字面
    "8 线程写入产物" 的偏差：段来源不同、归并覆盖的格式路径完全相同（段内布局
    与 sharded 段无任何差异）——此处记为计划期修正，review 时点名。
13. **老 `segments_N` 清理范围**：本系统 `IndexWriter::commit` 从不删旧代（index_writer.rs 无
    删除路径），多次 commit 后目录里可能有 `segments_1..segments_N` 多代共存；force_merge 提交
    新代后删除**全部旧代**（gen < 新 gen 的 `segments_*`），与 Java on-commit 清理对齐
    （spec §4.1 "旧 segments_N"）。

### Steps

- [ ] **Step 1: 纯函数 `merge_sorted_dicts` + `build_ord_remap` 失败测试**

  文件：`crates/core/src/merge.rs`（新建，先只有 `#[cfg(test)]` 与空壳）+
  `crates/core/src/lib.rs`（加 `pub mod merge;`）。

  设计要点（spec §5.1，全里程碑最易错点，先纯函数后格式层）：各段 SortedDV 字典各自
  按无符号字节序升序（`add_sorted_field` 的 debug_assert，doc_values.rs:136）；全局字典 =
  k-way 归并 + 去重（跨段重复值只占一个全局 ord）；重映射表 = 每段 `ord_old → ord_new`，
  由"段字典项在全局字典中的下标"直接给出（两指针，无需二分）。

  ```rust
  //! forceMerge(1) —— 格式级段归并（M6 spec §4）。总装见 `force_merge`；
  //! 本模块同时承载归并用的纯函数（先单测后接格式层，spec §5.1）。

  use std::io;

  use codec_lucene9::FSDirectory;

  use crate::IndexWriterConfig;

  /// 多路已序字典归并：各自无符号字节序升序、段内无重复的输入 → 全局升序去重字典。
  /// （SortedDocValuesWriter 全局 ord 分配的对偶操作；Lucene 由
  /// DocValuesConsumer.merge 的 TermsEnum 归并完成，本系统段内字典有序 ⇒ 纯函数。）
  pub(crate) fn merge_sorted_dicts(dicts: &[Vec<Vec<u8>>]) -> Vec<Vec<u8>> {
      todo!("Step 2")
  }

  /// 每段 ord_old → 全局 ord_new 重映射表。`dicts[i]` 的每一项必在 `global` 中
  /// （merge_sorted_dicts 的输出是输入的并集）。两指针：两字典均升序。
  pub(crate) fn build_ord_remap(dicts: &[Vec<Vec<u8>>], global: &[Vec<u8>]) -> Vec<Vec<u32>> {
      todo!("Step 2")
  }

  pub fn force_merge(_dir: &FSDirectory, _config: &IndexWriterConfig) -> io::Result<()> {
      todo!("Step 16")
  }

  #[cfg(test)]
  mod tests {
      use super::*;

      fn dict(items: &[&str]) -> Vec<Vec<u8>> {
          items.iter().map(|s| s.as_bytes().to_vec()).collect()
      }

      fn as_strings(d: &[Vec<u8>]) -> Vec<String> {
          d.iter()
              .map(|t| String::from_utf8(t.clone()).unwrap())
              .collect()
      }

      #[test]
      fn merge_dicts_dedup_and_order() {
          let dicts = vec![
              dict(&["apple", "cherry", "date"]),
              dict(&["banana", "cherry"]),
              dict(&["apple", "elderberry"]),
          ];
          let global = merge_sorted_dicts(&dicts);
          assert_eq!(
              as_strings(&global),
              vec!["apple", "banana", "cherry", "date", "elderberry"]
          );
      }

      #[test]
      fn merge_dicts_empty_inputs() {
          // 零段、全空段、空段混排都是合法输入（字段级空，关键代码事实 10）
          assert!(merge_sorted_dicts(&[]).is_empty());
          assert!(merge_sorted_dicts(&[vec![], vec![]]).is_empty());
          let dicts = vec![dict(&["a"]), vec![], dict(&["b"])];
          assert_eq!(as_strings(&merge_sorted_dicts(&dicts)), vec!["a", "b"]);
      }

      #[test]
      fn ord_remap_with_duplicates_and_empty_segment() {
          let dicts = vec![
              dict(&["apple", "cherry", "date"]),
              dict(&["banana", "cherry"]),
              vec![],
          ];
          let global = merge_sorted_dicts(&dicts);
          let remap = build_ord_remap(&dicts, &global);
          assert_eq!(remap.len(), 3);
          assert_eq!(remap[0], vec![0, 2, 3]); // apple→0 cherry→2 date→3
          assert_eq!(remap[1], vec![1, 2]); // banana→1 cherry→2
          assert!(remap[2].is_empty());
      }

      #[test]
      fn ord_remap_single_segment_is_identity() {
          let dicts = vec![dict(&["a", "b", "c"])];
          let global = merge_sorted_dicts(&dicts);
          assert_eq!(build_ord_remap(&dicts, &global), vec![vec![0, 1, 2]]);
      }
  }
  ```

  lib.rs 改动（加在 `pub mod json;` 之后，保持字母序）：

  ```rust
  pub mod merge;
  ```

  merge.rs 头部 import 随步骤累加，终态清单（一次写全免反复回头）：

  ```rust
  use std::io;

  use codec_lucene9::field_infos::{FieldInfo, FieldInfos};
  use codec_lucene9::postings_read::NO_MORE_DOCS;
  use codec_lucene9::segment_info::SegmentInfo;
  use codec_lucene9::segment_infos::{SegmentCommitInfo, SegmentInfos};
  use codec_lucene9::{DocValuesType, FSDirectory, IndexOptions};

  use crate::IndexWriterConfig;

  #[cfg(test)]
  use crate::search::{Query, Searcher};
  #[cfg(test)]
  use crate::{Document, FieldSpec, FieldValue, IndexWriter, Schema};
  #[cfg(test)]
  use codec_lucene9::io::DataInput;
  #[cfg(test)]
  use codec_lucene9::segment_infos::random_id;
  #[cfg(test)]
  use std::fs;
  #[cfg(test)]
  use std::path::PathBuf;
  ```

  （`SegmentCommitInfo` 在 Step 11 测试用到；`SegmentInfo` 在 Step 16 用到；
  各归并函数内部的 codec writer/reader import 就近写在函数里，见 Step 12/14。）

  跑：

  ```
  $ cargo test -p rustlucene-core merge:: 2>&1 | tail -5
  # 期望：4 个测试全部 FAILED（todo!() panic）——红
  ```

- [ ] **Step 2: 实现两个纯函数**

  `crates/core/src/merge.rs` 的两个 `todo!()` 换成：

  ```rust
  pub(crate) fn merge_sorted_dicts(dicts: &[Vec<Vec<u8>>]) -> Vec<Vec<u8>> {
      // k 路归并：每路游标取最小项；并列最小（跨段重复）全部推进但只收一份。
      let mut cursors = vec![0usize; dicts.len()];
      let mut global: Vec<Vec<u8>> = Vec::new();
      loop {
          let mut min: Option<&[u8]> = None;
          for (i, d) in dicts.iter().enumerate() {
              if let Some(term) = d.get(cursors[i]) {
                  min = Some(match min {
                      None => term,
                      Some(m) if term.as_slice() < m => term,
                      Some(m) => m,
                  });
              }
          }
          let Some(min) = min else { break };
          if global.last().map_or(true, |last| last.as_slice() != min) {
              global.push(min.to_vec());
          }
          for (i, d) in dicts.iter().enumerate() {
              if d.get(cursors[i]).map_or(false, |t| t.as_slice() == min) {
                  cursors[i] += 1;
              }
          }
      }
      global
  }

  pub(crate) fn build_ord_remap(dicts: &[Vec<Vec<u8>>], global: &[Vec<u8>]) -> Vec<Vec<u32>> {
      dicts
          .iter()
          .map(|d| {
              let mut remap = Vec::with_capacity(d.len());
              let mut g = 0usize;
              for term in d {
                  while g < global.len() && global[g].as_slice() != term.as_slice() {
                      g += 1;
                  }
                  assert!(g < global.len(), "term missing from merged dict");
                  remap.push(g as u32);
              }
              remap
          })
          .collect()
  }
  ```

  跑：

  ```
  $ cargo test -p rustlucene-core merge:: 2>&1 | tail -5
  # 期望：4 passed —— 绿
  $ cargo fmt
  $ git add crates/core/src/merge.rs crates/core/src/lib.rs
  $ git commit -m "feat: M6 T-C merge.rs 骨架 + SortedDV 字典归并/ord 重映射纯函数（spec §5.1 先行）"
  ```

- [ ] **Step 3: `StoredFieldsIndexReader`（.fdx/.fdm 读）失败测试**

  文件：`crates/codec-lucene9/src/stored_fields.rs` 测试模块。构造两个已知 chunk 数的段
  （`StoredFieldsWriter` 直写，与 flush 同格式），断言块数/每块 doc 数/字节区间与写侧记录
  一致，并验证区间切片的首两个 VInt 是 (docBase, code)。

  依据的布局（写侧 stored_fields.rs:482-573；对照 FieldsIndexWriter.finish :106-182）：
  `.fdm` = header("Lucene90FieldsIndexMeta",v1) → VInt chunkSize → int numDocs →
  int blockShift → int(totalChunks+1) → long docsStartPointer(.fdx 内 fp) →
  docs 的 DirectMonotonic meta（内联 .fdm，21B/块）→ long startPointersStartPointer →
  startPointers 的 DirectMonotonic meta → long startPointersEndPointer → long maxPointer →
  VLong numChunks/numDirtyChunks/numDirtyDocs → footer。`.fdx` =
  header("Lucene90FieldsIndexIdx",v0) → docs DM packed data → startPointers DM packed data →
  footer。DM meta 块数由 `(numValues-1)>>blockShift + 1` 推出（packed.rs:237-241），
  数据区长度 = 下一区起点 − 本区起点（末区用 `.fdx` 数据末尾，即 footer 前 16B）。

  测试代码（加进 `mod tests`）：

  ```rust
      use crate::io::DataInput;

      /// 写三个 chunk（2+2+1 docs），用新 reader 复读块索引。
      #[test]
      fn index_reader_chunk_layout() {
          let root = temp_dir("idxread");
          let dir = FSDirectory::open(&root).unwrap();
          let id = [9u8; 16];
          let mut w = StoredFieldsWriter::new(&dir, "_0", id, "").unwrap();
          // chunk 1: docs 0,1（每 doc 一个小字符串字段）
          for d in 0..5 {
              w.write_document(&[(0, StoredField::String(format!("doc-{d}")))])
                  .unwrap();
              if d == 1 || d == 3 {
                  w.force_flush_for_test(); // Step 4 在 impl 里新增的 #[cfg(test)] flush(true) 出口
              }
          }
          let stats = w.finish(5, &dir).unwrap();
          assert_eq!(stats.num_chunks, 3);

          let idx = StoredFieldsIndexReader::open(&dir, "_0", &id).unwrap();
          assert_eq!(idx.num_chunks(), 3);
          assert_eq!(idx.chunk_doc_count(0), 2);
          assert_eq!(idx.chunk_doc_count(1), 2);
          assert_eq!(idx.chunk_doc_count(2), 1);

          // 字节区间单调递增且末块终点 = maxPointer；逐块头部 (docBase, code) 校验
          let mut fdt = dir.open_input("_0.fdt").unwrap();
          let mut prev_end = 0;
          let mut doc_base = 0;
          for c in 0..idx.num_chunks() {
              let (start, end) = idx.chunk_byte_range(c);
              assert!(start >= prev_end && end > start);
              prev_end = end;
              fdt.seek(start).unwrap();
              assert_eq!(fdt.read_vint().unwrap(), doc_base);
              let code = fdt.read_vint().unwrap();
              assert_eq!(code >> 2, idx.chunk_doc_count(c));
              assert_eq!(code & 1, 0, "never sliced (small docs)");
              doc_base += idx.chunk_doc_count(c);
          }
          fs::remove_dir_all(&root).unwrap();
      }
  ```

  同时需要 writer 的测试出口（实现步一并加）：`flush(force)` 是私有的，chunk 边界由
  CHUNK_SIZE/MAX_DOCS_PER_CHUNK 触发，小文档单 chunk 无法测多块 ⇒ 加
  `pub(crate) fn force_flush_for_test(&mut self)`（`flush(true)` 的薄包装，cfg(test) 不限定——
  与 Java 测试用 reflection 触发 flush 不同，Rust 惯例是 pub(crate) 出口；放
  `#[cfg(test)]` 会让非测试代码无法调用，但本方法也只服务测试 ⇒ 用
  `#[cfg(test)] fn force_flush_for_test`）。

  跑：

  ```
  $ cargo test -p codec-lucene9 stored_fields::tests::index_reader_chunk_layout 2>&1 | tail -5
  # 期望：编译失败（StoredFieldsIndexReader / force_flush_for_test 不存在）——红
  ```


- [ ] **Step 4: 实现 `StoredFieldsIndexReader` + `force_flush_for_test` 测试出口**

  `crates/codec-lucene9/src/stored_fields.rs`。所有权设计：`DirectMonotonicReader<'a>`
  借用 `&'a [u8]`，让 reader 拥有数据会变自引用结构——所以 reader 拥有 `Vec<u8>`
  原件，访问时现场重建 DM 视图（`DirectMonotonicReader::new` 是纯解析无 IO，
  ~ns 级；与 M5 事实 5 "不存 view 每次重建" 同款决议）。`.fdm` 用
  `open_checksum_input` + `check_footer` 全量校验；`.fdx` 用 `open_input` +
  `check_index_header` + `check_footer_structure`（postings_read.rs:63-73 的既有
  模式），header 长度直接取 check 后的 `file_pointer()`——`codec_util.rs` 零改动
  （`index_header_length` 本就是 pub fn，stored_fields.rs:15 的 `#[cfg(test)]` 只是
  import 侧的门）。reader 里 `INDEX_CODEC_NAME`/`FDT_VERSION`/`FIELDS_INDEX_VERSION`/
  `CHUNK_SIZE`/`file_names` 都是同模块私有项，直接用。

  ```rust
  // 文件头部 import 追加：
  use crate::codec_util::{check_footer, check_footer_structure, check_index_header};
  use crate::io::{ChecksumIndexInput, DataInput};
  use crate::packed::DirectMonotonicReader;

  /// `.fdx/.fdm` 块索引读（FieldsIndexReader 对偶；Lucene90CompressingStoredFieldsReader
  /// 构造路径；M6 T-C stored 裸拷贝专用，spec §4.2/§4.3）。打开时解析全部元数据，
  /// 两个 DirectMonotonic 序列（docs 累计数 / chunk 起始 fp）的原件驻内存，
  /// get 时现场重建 view。
  pub struct StoredFieldsIndexReader {
      block_shift: u32,
      num_chunks: usize,
      docs_meta: Vec<u8>,
      docs_data: Vec<u8>,
      sp_meta: Vec<u8>,
      sp_data: Vec<u8>,
  }

  /// 从 .fdm 流内读一个 DirectMonotonic 的 meta 区（内联 21B/块；
  /// DirectMonotonicReader.Meta 构造的块数公式，packed.rs:237-241）。
  fn read_dm_meta(
      fdm: &mut ChecksumIndexInput,
      num_values: usize,
      block_shift: u32,
  ) -> io::Result<Vec<u8>> {
      let num_blocks = if num_values == 0 {
          0
      } else {
          (num_values - 1) >> block_shift
      } + 1;
      let mut meta = vec![0u8; num_blocks * DirectMonotonicReader::META_RECORD_BYTES];
      fdm.read_bytes(&mut meta)?;
      Ok(meta)
  }

  impl StoredFieldsIndexReader {
      /// 解析 .fdm 全部元数据 + 从 .fdx 切出两个 DM 数据区
      /// （布局对照写侧 finish，stored_fields.rs:482-573）。
      pub fn open(dir: &FSDirectory, segment: &str, segment_id: &[u8; 16]) -> io::Result<Self> {
          let [_fdt_name, fdx_name, fdm_name] = file_names(segment, "");
          let mut fdm = dir.open_checksum_input(&fdm_name)?;
          check_index_header(
              &mut fdm,
              &format!("{INDEX_CODEC_NAME}Meta"),
              FDT_VERSION,
              FDT_VERSION,
              segment_id,
              "",
          )?;
          let chunk_size = fdm.read_vint()?;
          if chunk_size != CHUNK_SIZE as i32 {
              return Err(io::Error::new(
                  io::ErrorKind::InvalidData,
                  format!("chunkSize {chunk_size} != {CHUNK_SIZE}"),
              ));
          }
          let num_docs = fdm.read_int()?;
          let block_shift = fdm.read_int()? as u32;
          let total_values = fdm.read_int()? as usize; // totalChunks + 1
          if total_values == 0 {
              return Err(io::Error::new(
                  io::ErrorKind::InvalidData,
                  "corrupt fdm: totalChunks + 1 == 0",
              ));
          }
          let docs_sp = fdm.read_long()? as u64;
          let docs_meta = read_dm_meta(&mut fdm, total_values, block_shift)?;
          let sp_sp = fdm.read_long()? as u64;
          let sp_meta = read_dm_meta(&mut fdm, total_values, block_shift)?;
          let sp_end = fdm.read_long()? as u64;
          let _max_pointer = fdm.read_long()? as u64;
          let _num_chunks = fdm.read_vlong()?;
          let _num_dirty_chunks = fdm.read_vlong()?;
          let _num_dirty_docs = fdm.read_vlong()?;
          check_footer(&mut fdm)?;

          // .fdx：header 之后是两段 DM packed data，sp_end 即数据区末尾（写侧
          // finish 随即 write_footer，stored_fields.rs:549-552）。
          let mut fdx_in = dir.open_input(&fdx_name)?;
          check_index_header(
              &mut fdx_in,
              &format!("{INDEX_CODEC_NAME}Idx"),
              FIELDS_INDEX_VERSION,
              FIELDS_INDEX_VERSION,
              segment_id,
              "",
          )?;
          let header_len = fdx_in.file_pointer();
          check_footer_structure(&fdx_in, fdx_in.length())?;
          let mut fdx = vec![0u8; (fdx_in.length() - header_len - 16) as usize]; // 16 = footer
          fdx_in.read_bytes(&mut fdx)?;
          let rel = |fp: u64| (fp - header_len) as usize;
          let reader = StoredFieldsIndexReader {
              block_shift,
              num_chunks: total_values - 1,
              docs_data: fdx[rel(docs_sp)..rel(sp_sp)].to_vec(),
              docs_meta,
              sp_data: fdx[rel(sp_sp)..rel(sp_end)].to_vec(),
              sp_meta,
          };
          // docs DM 末值 == numDocs（写侧 debug_assert，stored_fields.rs:523）
          debug_assert_eq!(reader.docs_dm().get(total_values as u64 - 1), num_docs as u64);
          Ok(reader)
      }

      fn docs_dm(&self) -> DirectMonotonicReader<'_> {
          DirectMonotonicReader::new(
              &self.docs_meta,
              &self.docs_data,
              self.num_chunks + 1,
              self.block_shift,
          )
          .expect("meta length checked at open")
      }

      fn sp_dm(&self) -> DirectMonotonicReader<'_> {
          DirectMonotonicReader::new(
              &self.sp_meta,
              &self.sp_data,
              self.num_chunks + 1,
              self.block_shift,
          )
          .expect("meta length checked at open")
      }

      pub fn num_chunks(&self) -> usize {
          self.num_chunks
      }

      /// chunk 内文档数 = docsDM[chunk+1] - docsDM[chunk]。
      pub fn chunk_doc_count(&self, chunk: usize) -> i32 {
          let dm = self.docs_dm();
          (dm.get(chunk as u64 + 1) - dm.get(chunk as u64)) as i32
      }

      /// chunk 在 .fdt 中的字节区间 [start, end)；末块 end == maxPointer
      /// （sp DM 末值即 maxPointer，写侧 stored_fields.rs:536-540）。
      pub fn chunk_byte_range(&self, chunk: usize) -> (u64, u64) {
          let sp = self.sp_dm();
          (sp.get(chunk as u64), sp.get(chunk as u64 + 1))
      }
  }

  #[cfg(test)] 测试出口加进**既有** `impl StoredFieldsWriter` 块内（finish 旁边）：

  ```rust
      #[cfg(test)]
      fn force_flush_for_test(&mut self) {
          self.flush(true).unwrap();
      }
  ```

  测试模块补 import 与 helper（现存测试无目录用法）：

  ```rust
      use crate::directory::FSDirectory;
      use std::fs;
      use std::path::PathBuf;

      fn temp_dir(tag: &str) -> PathBuf {
          let dir = std::env::temp_dir().join(format!(
              "codec-lucene9-stored-{}-{}",
              tag,
              std::process::id()
          ));
          let _ = fs::remove_dir_all(&dir);
          dir
      }
  ```

  跑：

  ```
  $ cargo test -p codec-lucene9 stored_fields:: 2>&1 | tail -5
  # 期望：全部通过（含 Step 3 新测试 + 既有测试不回归）——绿
  $ cargo fmt
  $ git add crates/codec-lucene9/src/stored_fields.rs
  $ git commit -m "feat: M6 T-C .fdx/.fdm 块索引读 StoredFieldsIndexReader（DirectMonotonic 复用 packed 读侧）"
  ```

- [ ] **Step 5: `StoredFieldsWriter::append_raw_chunk` 失败测试**

  文件：`crates/codec-lucene9/src/stored_fields.rs` 测试模块。端到端裸拷贝：段 A（3 chunks，
  2+2+1 docs）逐 chunk 切开（头两 VInt 校验+剥除，其余字节原样）追加进段 B；段 B 用
  `StoredFieldsIndexReader` 复读断言 chunk 布局 (docBase 重定基为 0/2/4)，再用 Java 语义
  的区间字节比对断言 payload 逐字节一致。

  ```rust
      /// copyChunks 主路径（Lucene90CompressingStoredFieldsWriter.java:520-595）：
      /// 头两个 VInt 重写（docBase 重定基），其余字节原样。
      #[test]
      fn append_raw_chunk_rebases_doc_base() {
          let root = temp_dir("rawcopy");
          let dir = FSDirectory::open(&root).unwrap();
          let id_a = [1u8; 16];
          let mut wa = StoredFieldsWriter::new(&dir, "_a", id_a, "").unwrap();
          for d in 0..5 {
              wa.write_document(&[(0, StoredField::String(format!("payload-{d}")))])
                  .unwrap();
              if d == 1 || d == 3 {
                  wa.force_flush_for_test();
              }
          }
          wa.finish(5, &dir).unwrap();

          let idx = StoredFieldsIndexReader::open(&dir, "_a", &id_a).unwrap();
          let id_b = [2u8; 16];
          let mut wb = StoredFieldsWriter::new(&dir, "_b", id_b, "").unwrap();
          let mut fdt = dir.open_input("_a.fdt").unwrap();
          for c in 0..idx.num_chunks() {
              let (start, end) = idx.chunk_byte_range(c);
              fdt.seek(start).unwrap();
              let _src_base = fdt.read_vint().unwrap();
              let code = fdt.read_vint().unwrap();
              let mut payload = vec![0u8; (end - fdt.file_pointer()) as usize];
              fdt.read_bytes(&mut payload).unwrap();
              wb.append_raw_chunk(idx.chunk_doc_count(c), code, &payload)
                  .unwrap();
          }
          wb.finish(5, &dir).unwrap();

          // 段 B 索引：3 chunks、doc 数 2/2/1、docBase 已重定基
          let idx_b = StoredFieldsIndexReader::open(&dir, "_b", &id_b).unwrap();
          assert_eq!(idx_b.num_chunks(), 3);
          let mut fdt_b = dir.open_input("_b.fdt").unwrap();
          let mut doc_base = 0;
          for c in 0..idx_b.num_chunks() {
              assert_eq!(idx_b.chunk_doc_count(c), idx.chunk_doc_count(c));
              let (start_b, end_b) = idx_b.chunk_byte_range(c);
              fdt_b.seek(start_b).unwrap();
              assert_eq!(fdt_b.read_vint().unwrap(), doc_base, "chunk {c} docBase rebased");
              let code_b = fdt_b.read_vint().unwrap();
              // payload（header 之后全部字节）与源段逐字节一致
              let (start_a, end_a) = idx.chunk_byte_range(c);
              fdt.seek(start_a).unwrap();
              fdt.read_vint().unwrap();
              let code_a = fdt.read_vint().unwrap();
              assert_eq!(code_a, code_b);
              let mut pa = vec![0u8; (end_a - fdt.file_pointer()) as usize];
              fdt.read_bytes(&mut pa).unwrap();
              let mut pb = vec![0u8; (end_b - fdt_b.file_pointer()) as usize];
              fdt_b.read_bytes(&mut pb).unwrap();
              assert_eq!(pa, pb, "chunk {c} payload byte-identical");
              doc_base += idx_b.chunk_doc_count(c);
          }
          fs::remove_dir_all(&root).unwrap();
      }
  ```

  跑：

  ```
  $ cargo test -p codec-lucene9 stored_fields::tests::append_raw_chunk_rebases_doc_base 2>&1 | tail -5
  # 期望：编译失败（append_raw_chunk 不存在）——红
  ```

- [ ] **Step 6: 实现 `append_raw_chunk`**

  `StoredFieldsWriter` 加方法（放在 `flush` 之后；复用 `finish` 全部收尾逻辑——
  chunk_num_docs/chunk_start_pointers/num_dirty_* 的记账与 `flush` 完全一致，
  这就是裸拷贝能白嫖 .fdx/.fdm 重建的关键）：

  ```rust
      /// M6 T-C：裸 chunk 追加（Lucene90CompressingStoredFieldsWriter.copyChunks
      /// :552-595 的写侧一半）。`code` 原样携带源 chunk 的 numDocs<<2|dirty|sliced
      /// 位；docBase 重写为当前 doc_base（rebase，:565-568）。`payload` = 源 chunk
      /// 去掉 (docBase, code) 两个 VInt 后的全部字节（numStoredFields/lengths/LZ4
      /// 数据，逐字节不解压）。调用后内部 doc 缓冲必须恒空——与 write_field 的
      /// 文档级写入路径互斥（本系统归并只用裸路径）。
      pub fn append_raw_chunk(&mut self, num_docs: i32, code: i32, payload: &[u8]) -> io::Result<()> {
          assert_eq!(
              self.num_buffered_docs, 0,
              "raw chunk append never mixes with doc-level writes"
          );
          assert_eq!(code >> 2, num_docs, "code numDocs mismatch");
          self.num_chunks += 1;
          if code & 2 != 0 {
              // dirty bit（force flush 的 chunk）：记账与 flush(true) 一致 (:238-241)
              self.num_dirty_chunks += 1;
              self.num_dirty_docs += num_docs as i64;
          }
          self.chunk_num_docs.push(num_docs);
          self.chunk_start_pointers
              .push(self.fields_stream.file_pointer() as i64);
          self.total_docs_in_chunks += num_docs as i64;
          self.fields_stream.write_vint(self.doc_base)?; // rebase
          self.fields_stream.write_vint(code)?;
          self.fields_stream.write_bytes(payload)?;
          self.doc_base += num_docs;
          Ok(())
      }
  ```

  跑：

  ```
  $ cargo test -p codec-lucene9 stored_fields:: 2>&1 | tail -5
  # 期望：全部通过 —— 绿
  $ cargo fmt
  $ git add crates/codec-lucene9/src/stored_fields.rs
  $ git commit -m "feat: M6 T-C stored 块级裸拷贝写侧 append_raw_chunk（docBase 重定基，payload 逐字节）"
  ```

- [ ] **Step 7: `DocValuesReader`（.dvm/.dvd 顺序读）失败测试**

  文件：`crates/codec-lucene9/src/doc_values_read.rs`（新建，测试内联）+
  `crates/codec-lucene9/src/lib.rs`（`pub mod doc_values_read;`，按字母序插在
  `pub mod doc_values;` 后）。

  测试设计：用既有 `DocValuesWriter` 写出已知字段，新 reader 复读。覆盖：稠密常量
  Numeric（bpv 0 分支）、稀疏 Numeric（SPARSE+DENSE+ALL 三种 DISI 块 + jump table）、
  空 Numeric（docsWithField = -2 分支）、Sorted（150 词字典跨 3 个 64 项块 + 部分 doc
  无值）、空 Sorted。每个用例值/doc 序逐点断言。

  ```rust
  //! Lucene90 DocValues 顺序读（Lucene90DocValuesProducer :197-299 的归并子集；
  //! M6 T-C，spec §4.3）。只服务 forceMerge 的全量顺序遍历：无随机点查、
  //! 无跳表加速（IndexedDISI jump table 解析但顺序解码块体）。
  //! 布局 ground truth：docs/format-notes-docvalues.md + doc_values.rs 写侧注释。

  use std::io;

  use crate::codec_util::{check_footer, check_index_header};
  use crate::directory::FSDirectory;
  use crate::io::{ChecksumIndexInput, DataInput};
  use crate::packed::{DirectMonotonicReader, DirectReader};

  // （实现见 Step 8；先写测试）
  pub struct DocValuesReader;

  #[cfg(test)]
  mod tests {
      use super::*;
      use crate::doc_values::DocValuesWriter;
      use std::fs;
      use std::path::PathBuf;

      const SEGMENT_ID: [u8; 16] = [0x5A; 16];
      const SUFFIX: &str = "Lucene90_0";

      fn temp_dir(tag: &str) -> PathBuf {
          let dir = std::env::temp_dir().join(format!(
              "codec-lucene9-dvr-{}-{}",
              tag,
              std::process::id()
          ));
          let _ = fs::remove_dir_all(&dir);
          dir
      }

      fn write_index(tag: &str, f: impl FnOnce(&mut DocValuesWriter)) -> PathBuf {
          let root = temp_dir(tag);
          let dir = FSDirectory::open(&root).unwrap();
          let mut w = DocValuesWriter::new(&dir, "_0", &SEGMENT_ID, SUFFIX).unwrap();
          f(&mut w);
          w.finish().unwrap();
          root
      }

      #[test]
      fn numeric_dense_sparse_empty() {
          let max_doc = 200_000u32;
          // field 0: 稠密常量（bpv 0、无 DISI）
          let constant: Vec<(u32, i64)> = (0..1000).map(|d| (d, 42)).collect();
          // field 1: 稀疏三形态 DISI（照抄 doc_values.rs 既有测试的分布）
          let mut sparse: Vec<(u32, i64)> = vec![(5, -7), (100, 8), (4095, 9)];
          for i in 0..5000u32 {
              sparse.push((65536 + i, i as i64 * 3));
          }
          for i in 0..65536u32 {
              sparse.push((131072 + i, -1));
          }
          // field 2: 全空（docsWithField = -2 分支）
          let root = write_index("num", |w| {
              w.add_numeric_field(0, max_doc, &constant).unwrap();
              w.add_numeric_field(1, max_doc, &sparse).unwrap();
              w.add_numeric_field(2, max_doc, &[]).unwrap();
          });
          let dir = FSDirectory::open(&root).unwrap();
          let r = DocValuesReader::open(&dir, "_0", &SEGMENT_ID, SUFFIX).unwrap();
          assert_eq!(r.numeric_values(0).unwrap(), constant);
          assert_eq!(r.numeric_values(1).unwrap(), sparse);
          assert_eq!(r.numeric_values(2).unwrap(), Vec::<(u32, i64)>::new());
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn sorted_dict_and_ords() {
          // 150 词 → 3 个 64 项块；ords 部分 doc 无值（docsWithField 稀疏）
          let dict: Vec<String> = (0..150).map(|i| format!("term-{i:04}")).collect();
          let dict_refs: Vec<&[u8]> = dict.iter().map(|s| s.as_bytes()).collect();
          let ords: Vec<(u32, u32)> = (0..300u32)
              .filter(|d| d % 3 != 0) // 200/300 docs 有值 → DISI 稀疏路径
              .map(|d| (d, d % 150))
              .collect();
          let root = write_index("sorted", |w| {
              w.add_sorted_field(0, 300, &dict_refs, &ords).unwrap();
          });
          let dir = FSDirectory::open(&root).unwrap();
          let r = DocValuesReader::open(&dir, "_0", &SEGMENT_ID, SUFFIX).unwrap();
          let got_dict = r.sorted_dict(0).unwrap();
          assert_eq!(got_dict.len(), 150);
          for (got, want) in got_dict.iter().zip(dict.iter()) {
              assert_eq!(got, want.as_bytes());
          }
          assert_eq!(r.sorted_ords(0).unwrap(), ords);
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn empty_sorted_field() {
          let root = write_index("empty", |w| {
              w.add_sorted_field(1, 10, &[], &[]).unwrap();
          });
          let dir = FSDirectory::open(&root).unwrap();
          let r = DocValuesReader::open(&dir, "_0", &SEGMENT_ID, SUFFIX).unwrap();
          assert!(r.sorted_dict(1).unwrap().is_empty());
          assert!(r.sorted_ords(1).unwrap().is_empty());
          fs::remove_dir_all(&root).unwrap();
      }
  }
  ```

  lib.rs：

  ```rust
  pub mod doc_values_read;
  ```

  跑：

  ```
  $ cargo test -p codec-lucene9 doc_values_read:: 2>&1 | tail -5
  # 期望：编译失败（DocValuesReader 方法不存在）——红
  ```

- [ ] **Step 8: 实现 `DocValuesReader`**

  实现（`crates/codec-lucene9/src/doc_values_read.rs`，替换 Step 7 的空壳 struct。
  解码逻辑与 doc_values.rs 既有测试里的 ByteReader/decode_disi/decode_terms 同款，
  但那套是断言型测试脚手架——生产版用 `IndexInput`/`ChecksumIndexInput` +
  `packed::{DirectReader, DirectMonotonicReader}`，逐条 file:line 注释）。
  **前置一行改动**：`doc_values.rs` 的 `DATA_CODEC`/`META_CODEC`/`VERSION`/
  `TYPE_NUMERIC`/`TYPE_SORTED`/`DIRECT_MONOTONIC_BLOCK_SHIFT`/
  `TERMS_DICT_BLOCK_SIZE`/`TERMS_DICT_REVERSE_INDEX_SIZE`/`DISI_BLOCK_SIZE`/
  `DISI_MAX_ARRAY_LENGTH`/`DISI_SENTINEL_BLOCK` 由私有 `const` 改 `pub(crate) const`
  （11 处一行级，数值不重抄）。完整文件：

  ```rust
  //! Lucene90 DocValues 顺序读（Lucene90DocValuesProducer :197-299 的归并子集；
  //! M6 T-C，spec §4.3）。只服务 forceMerge 的全量顺序遍历：无随机点查、
  //! 无跳表加速（IndexedDISI jump table 与 terms reverse index 解析但跳过）。
  //! 布局 ground truth：docs/format-notes-docvalues.md + doc_values.rs 写侧注释。

  use std::io;

  use crate::codec_util::{check_footer, check_footer_structure, check_index_header, corrupt};
  use crate::directory::FSDirectory;
  use crate::doc_values::{
      DATA_CODEC, DIRECT_MONOTONIC_BLOCK_SHIFT, DISI_BLOCK_SIZE, DISI_MAX_ARRAY_LENGTH,
      DISI_SENTINEL_BLOCK, META_CODEC, TERMS_DICT_BLOCK_SIZE, TERMS_DICT_REVERSE_INDEX_SIZE,
      TYPE_NUMERIC, TYPE_SORTED, VERSION,
  };
  use crate::io::{ChecksumIndexInput, DataInput, IndexInput};
  use crate::packed::{DirectMonotonicReader, DirectReader};

  /// Lucene90DocValuesProducer.readNumeric (:197-224) 的归并子集。
  struct NumericMeta {
      docs_offset: i64, // -2 = 全空；-1 = 稠密；否则 DISI 区起点（.dvd 绝对 fp）
      docs_length: i64,
      num_values: u64,
      bpv: u8,
      min: i64,
      gcd: i64,
      values_offset: i64,
      values_length: i64,
  }

  /// readTermDict (:278-299)：ords 子条目 + terms dict 元数据。
  /// terms reverse index 的偏移量读入即弃（归并不需要点查）。
  struct SortedMeta {
      ords: NumericMeta,
      dict_size: u64,
      block_shift: u32,
      addresses_meta: Vec<u8>, // DirectMonotonic meta（.dvm 内联 21B/块）
      terms_data_offset: i64,
      terms_data_length: i64,
      terms_addresses_offset: i64,
      terms_addresses_length: i64,
  }

  enum DvEntry {
      Numeric(NumericMeta),
      Sorted(SortedMeta),
  }

  pub struct DocValuesReader {
      /// .dvd header 之后的全部字节（footer 除外）；归并逐字段顺序消费。
      dvd: Vec<u8>,
      /// .dvd index header 长度：meta 里的 offset 是绝对 fp，切片时减之。
      header_len: u64,
      entries: Vec<(i32, DvEntry)>,
  }

  /// readNumeric (:197-224)。tableSize 恒 -1、valueJumpTableOffset 恒 -1
  /// （写侧 doc_values.rs:221/238）；其他值是非本系统产物，拒绝。
  fn read_numeric_meta(dvm: &mut ChecksumIndexInput) -> io::Result<NumericMeta> {
      let docs_offset = dvm.read_long()?;
      let docs_length = dvm.read_long()?;
      let _jump_table_entry_count = dvm.read_short()?;
      let _dense_rank_power = dvm.read_byte()?;
      let num_values = dvm.read_long()? as u64;
      let table_size = dvm.read_int()?;
      if table_size != -1 {
          return Err(corrupt(format!("tableSize {table_size} != -1 (not our writer)")));
      }
      let bpv = dvm.read_byte()?;
      let min = dvm.read_long()?;
      let gcd = dvm.read_long()?;
      let values_offset = dvm.read_long()?;
      let values_length = dvm.read_long()?;
      let value_jump_table_offset = dvm.read_long()?;
      if value_jump_table_offset != -1 {
          return Err(corrupt("valueJumpTable present (not our writer)"));
      }
      Ok(NumericMeta {
          docs_offset,
          docs_length,
          num_values,
          bpv,
          min,
          gcd,
          values_offset,
          values_length,
      })
  }

  /// readTermDict (:278-299)。reverse index 的 DM meta 内联在 .dvm——
  /// 必须读过（字节数按块数公式推出）才能到下一字段条目。
  fn read_terms_meta(dvm: &mut ChecksumIndexInput, ords: NumericMeta) -> io::Result<SortedMeta> {
      let dict_size = dvm.read_vlong()? as u64;
      let block_shift = dvm.read_int()? as u32;
      if block_shift != DIRECT_MONOTONIC_BLOCK_SHIFT {
          return Err(corrupt(format!("terms dict blockShift {block_shift}")));
      }
      let num_addr = (dict_size as usize).div_ceil(TERMS_DICT_BLOCK_SIZE);
      let mut addresses_meta = vec![0u8; dm_meta_len(num_addr, block_shift)];
      dvm.read_bytes(&mut addresses_meta)?;
      let _max_term_length = dvm.read_int()?;
      let _max_block_length = dvm.read_int()?;
      let terms_data_offset = dvm.read_long()?;
      let terms_data_length = dvm.read_long()?;
      let terms_addresses_offset = dvm.read_long()?;
      let terms_addresses_length = dvm.read_long()?;
      let index_shift = dvm.read_int()? as u32;
      if index_shift != 10 {
          return Err(corrupt(format!("terms index shift {index_shift}")));
      }
      let num_index = 1 + (dict_size as usize).div_ceil(TERMS_DICT_REVERSE_INDEX_SIZE);
      let mut skipped = vec![0u8; dm_meta_len(num_index, block_shift)];
      dvm.read_bytes(&mut skipped)?; // reverse index DM meta，读入即弃
      let _terms_index_offset = dvm.read_long()?;
      let _terms_index_length = dvm.read_long()?;
      let _terms_index_addresses_offset = dvm.read_long()?;
      let _terms_index_addresses_length = dvm.read_long()?;
      Ok(SortedMeta {
          ords,
          dict_size,
          block_shift,
          addresses_meta,
          terms_data_offset,
          terms_data_length,
          terms_addresses_offset,
          terms_addresses_length,
      })
  }

  /// DirectMonotonic meta 内联字节数（块数公式，packed.rs:237-241 × 21B/块）。
  fn dm_meta_len(num_values: usize, block_shift: u32) -> usize {
      let num_blocks = if num_values == 0 {
          0
      } else {
          (num_values - 1) >> block_shift
      } + 1;
      num_blocks * DirectMonotonicReader::META_RECORD_BYTES
  }

  impl DocValuesReader {
      /// 解析 .dvm 全部字段条目 + 校验 .dvd header/footer
      /// （Lucene90DocValuesProducer 构造 :168-195 的精简版）。
      pub fn open(
          dir: &FSDirectory,
          segment: &str,
          segment_id: &[u8; 16],
          suffix: &str,
      ) -> io::Result<Self> {
          let [dvd_name, dvm_name] = crate::doc_values::file_names(segment, suffix);
          let mut dvm = dir.open_checksum_input(&dvm_name)?;
          check_index_header(&mut dvm, META_CODEC, VERSION, VERSION, segment_id, suffix)?;
          let mut entries = Vec::new();
          loop {
              let field_number = dvm.read_int()?;
              if field_number == -1 {
                  break; // EOF marker（写侧 doc_values.rs:163）
              }
              match dvm.read_byte()? {
                  TYPE_NUMERIC => {
                      let m = read_numeric_meta(&mut dvm)?;
                      entries.push((field_number, DvEntry::Numeric(m)));
                  }
                  TYPE_SORTED => {
                      let ords = read_numeric_meta(&mut dvm)?;
                      let m = read_terms_meta(&mut dvm, ords)?;
                      entries.push((field_number, DvEntry::Sorted(m)));
                  }
                  t => return Err(corrupt(format!("unsupported DV type {t}"))),
              }
          }
          check_footer(&mut dvm)?;

          let mut dvd_in = dir.open_input(&dvd_name)?;
          check_index_header(&mut dvd_in, DATA_CODEC, VERSION, VERSION, segment_id, suffix)?;
          let header_len = dvd_in.file_pointer();
          check_footer_structure(&dvd_in, dvd_in.length())?;
          let mut dvd = vec![0u8; (dvd_in.length() - header_len - 16) as usize]; // 16 = footer
          dvd_in.read_bytes(&mut dvd)?;
          Ok(DocValuesReader {
              dvd,
              header_len,
              entries,
          })
      }

      /// meta 里的 offset 是 .dvd 绝对 fp；self.dvd 以 header 末尾为 0 基。
      fn slice(&self, offset: i64, length: i64) -> &[u8] {
          let start = (offset as u64 - self.header_len) as usize;
          &self.dvd[start..start + length as usize]
      }

      fn numeric_meta(&self, field_number: i32) -> Option<&NumericMeta> {
          self.entries
              .iter()
              .find(|(n, _)| *n == field_number)
              .and_then(|(_, e)| match e {
                  DvEntry::Numeric(m) => Some(m),
                  DvEntry::Sorted(_) => None,
              })
      }

      fn sorted_meta(&self, field_number: i32) -> Option<&SortedMeta> {
          self.entries
              .iter()
              .find(|(n, _)| *n == field_number)
              .and_then(|(_, e)| match e {
                  DvEntry::Sorted(m) => Some(m),
                  DvEntry::Numeric(_) => None,
              })
      }

      /// docsWithField（IndexedDISI 顺序解码，IndexedDISI.java:102-254）：
      /// docs_offset==-2 → 空；==-1 → 0..num_values（稠密）；否则逐块——块头
      /// LE short blockID + LE short cardinality-1；SPARSE（≤4095：LE short
      /// 低 16 位）、DENSE（256B rank 跳过 + 1024 LE long 位图展开）、
      /// ALL（==65536：无 payload）；sentinel 块（blockID == 0x7FFF）止；
      /// jump table 在块区末尾，顺序读不消费。
      fn read_docs_with_field(&self, m: &NumericMeta) -> io::Result<Vec<u32>> {
          if m.docs_offset == -2 {
              return Ok(Vec::new());
          }
          if m.docs_offset == -1 {
              return Ok((0..m.num_values as u32).collect());
          }
          let region = self.slice(m.docs_offset, m.docs_length);
          let le_u16 = |p: usize| u16::from_le_bytes(region[p..p + 2].try_into().unwrap());
          let mut docs = Vec::with_capacity(m.num_values as usize);
          let mut pos = 0usize;
          loop {
              let block_id = le_u16(pos) as u32;
              let cardinality = le_u16(pos + 2) as u32 + 1;
              pos += 4;
              if block_id == DISI_SENTINEL_BLOCK {
                  break;
              }
              if cardinality <= DISI_MAX_ARRAY_LENGTH {
                  for _ in 0..cardinality {
                      docs.push((block_id << 16) | le_u16(pos) as u32);
                      pos += 2;
                  }
              } else if cardinality == DISI_BLOCK_SIZE {
                  docs.extend((0..DISI_BLOCK_SIZE).map(|i| (block_id << 16) | i));
              } else {
                  pos += 256; // rank table
                  for word_index in 0..1024usize {
                      let mut w =
                          u64::from_le_bytes(region[pos..pos + 8].try_into().unwrap());
                      pos += 8;
                      while w != 0 {
                          let bit = w.trailing_zeros();
                          docs.push((block_id << 16) | ((word_index as u32) << 6) | bit);
                          w &= w - 1;
                      }
                  }
              }
          }
          debug_assert_eq!(docs.len() as u64, m.num_values);
          Ok(docs)
      }

      /// 值流：bpv==0 → vec![min; num_values]（producer :487-493）；否则
      /// DirectReader 逐值 `min + gcd * get(i)`（:527-534；gcd 恒 1 按通用解）。
      fn read_values(&self, m: &NumericMeta) -> Vec<i64> {
          if m.bpv == 0 {
              return vec![m.min; m.num_values as usize];
          }
          let reader =
              DirectReader::new(self.slice(m.values_offset, m.values_length), m.bpv as u32, 0)
                  .expect("writer-supported bpv");
          (0..m.num_values)
              .map(|i| (reader.get(i) as i64).wrapping_mul(m.gcd).wrapping_add(m.min))
              .collect()
      }

      /// 逐 doc (doc, value)，doc 升序。全空 → 空 Vec。
      pub fn numeric_values(&self, field_number: i32) -> io::Result<Vec<(u32, i64)>> {
          let Some(m) = self.numeric_meta(field_number) else {
              return Err(io::Error::new(
                  io::ErrorKind::NotFound,
                  format!("no NUMERIC DV entry for field {field_number}"),
              ));
          };
          let docs = self.read_docs_with_field(m)?;
          let values = self.read_values(m);
          debug_assert_eq!(docs.len(), values.len());
          Ok(docs.into_iter().zip(values).collect())
      }

      /// 逐 doc (doc, ord)，doc 升序：ords 子条目走 numeric 同一路径。
      pub fn sorted_ords(&self, field_number: i32) -> io::Result<Vec<(u32, u32)>> {
          let Some(s) = self.sorted_meta(field_number) else {
              return Err(io::Error::new(
                  io::ErrorKind::NotFound,
                  format!("no SORTED DV entry for field {field_number}"),
              ));
          };
          let docs = self.read_docs_with_field(&s.ords)?;
          let values = self.read_values(&s.ords);
          Ok(docs
              .into_iter()
              .zip(values)
              .map(|(d, o)| (d, o as u32))
              .collect())
      }

      /// terms dict 全量展开：64 项/块，块首词 verbatim（VInt 长度 + 字节），
      /// 其余在 `VInt uncompressedLength + LZ4 流` 内前缀压缩（token 低 4 位
      /// prefix（15 ⇒ +VInt 续）、高 4 位 suffix-1（=15 ⇒ suffix = 16+VInt）——
      /// 写侧 doc_values.rs:280-291 的逆；块地址 DirectMonotonic :578）。
      pub fn sorted_dict(&self, field_number: i32) -> io::Result<Vec<Vec<u8>>> {
          let Some(s) = self.sorted_meta(field_number) else {
              return Err(io::Error::new(
                  io::ErrorKind::NotFound,
                  format!("no SORTED DV entry for field {field_number}"),
              ));
          };
          let num_blocks = (s.dict_size as usize).div_ceil(TERMS_DICT_BLOCK_SIZE);
          let addrs = DirectMonotonicReader::new(
              &s.addresses_meta,
              self.slice(s.terms_addresses_offset, s.terms_addresses_length),
              num_blocks,
              s.block_shift,
          )?;
          let data = self.slice(s.terms_data_offset, s.terms_data_length);
          let mut terms = Vec::with_capacity(s.dict_size as usize);
          for b in 0..num_blocks {
              let start = addrs.get(b as u64) as usize;
              let end = if b + 1 < num_blocks {
                  addrs.get(b as u64 + 1) as usize
              } else {
                  data.len()
              };
              let region = &data[start..end];
              let mut r = IndexInput::in_memory(region.to_vec());
              let first_len = r.read_vint()? as usize;
              let mut first = vec![0u8; first_len];
              r.read_bytes(&mut first)?;
              terms.push(first.clone());
              let block_count =
                  (s.dict_size as usize - b * TERMS_DICT_BLOCK_SIZE).min(TERMS_DICT_BLOCK_SIZE);
              if block_count > 1 {
                  let uncompressed = r.read_vint()? as usize;
                  let mut compressed = vec![0u8; region.len() - r.file_pointer() as usize];
                  r.read_bytes(&mut compressed)?;
                  let decompressed = lz4::block::decompress(&compressed, Some(uncompressed as i32))
                      .map_err(|e| corrupt(format!("terms dict lz4: {e}")))?;
                  let mut dr = IndexInput::in_memory(decompressed);
                  let mut prev = first;
                  for _ in 1..block_count {
                      let token = dr.read_byte()? as usize;
                      let mut prefix = token & 0x0F;
                      let mut suffix = 1 + (token >> 4);
                      if prefix == 15 {
                          prefix += dr.read_vint()? as usize;
                      }
                      if suffix == 16 {
                          suffix += dr.read_vint()? as usize;
                      }
                      let mut sfx = vec![0u8; suffix];
                      dr.read_bytes(&mut sfx)?;
                      let mut term = prev[..prefix].to_vec();
                      term.extend_from_slice(&sfx);
                      prev = term.clone();
                      terms.push(term);
                  }
              }
          }
          Ok(terms)
      }
  }
  ```

  LZ4 用 `lz4::block::decompress`（codec-lucene9 既有依赖，与写侧同 crate 特性）。
  `corrupt` 是 `codec_util::corrupt`（postings_read.rs:6 同款 import）。

  跑：

  ```
  $ cargo test -p codec-lucene9 doc_values_read:: 2>&1 | tail -5
  # 期望：3 个测试全过 —— 绿
  $ cargo test -p codec-lucene9 2>&1 | tail -3
  # 期望：全 crate 无回归 —— 绿
  $ cargo fmt
  $ git add crates/codec-lucene9/src/doc_values_read.rs crates/codec-lucene9/src/doc_values.rs crates/codec-lucene9/src/lib.rs
  $ git commit -m "feat: M6 T-C DV 顺序读 DocValuesReader（Numeric 值/DISI docs + Sorted 字典/ords，归并专用）"
  ```


- [ ] **Step 9: field infos 一致性断言失败测试（FieldInfo derive 先行）**

  文件：`crates/core/src/merge.rs`。纯函数，继续 TDD。

  spec §4.2："各段 `.fnm` 逐字段断言一致（同源写出的既有不变量），直接复用；不一致
  报错，不做全局重编号。"FieldInfo 当前无 PartialEq——本步先在
  `crates/codec-lucene9/src/field_infos.rs:57` 的 `pub struct FieldInfo` 上加
  `#[derive(Clone, PartialEq, Eq, Debug)]`（一行；所有字段类型已支持 Eq：
  枚举四个均已 derive，BTreeMap/String/基本类型原生）。

  merge.rs 加空壳（实现进 Step 10）：

  ```rust
  /// spec §4.2：各段 field infos 必须逐字段一致（同源 IndexWriter 产物的既有
  /// 不变量）；不一致即报错，不做全局重编号（MergeState.fieldInfos 的
  /// 同名合并在本系统是恒等）。
  pub(crate) fn assert_field_infos_consistent(all: &[FieldInfos]) -> io::Result<()> {
      let _ = all;
      todo!("Step 10")
  }
  ```

  测试（merge.rs `mod tests` 追加；用 `FieldInfo::stored` 构造再改属性制造不一致）：

  ```rust
      use codec_lucene9::field_infos::{DocValuesType, IndexOptions};

      fn fis(specs: &[(&str, IndexOptions, DocValuesType)]) -> FieldInfos {
          FieldInfos::new(
              specs
                  .iter()
                  .enumerate()
                  .map(|(i, &(name, io_opt, dv))| {
                      let mut fi = FieldInfo::stored(name, i as i32);
                      fi.index_options = io_opt;
                      fi.omit_norms = io_opt != IndexOptions::None;
                      fi.doc_values_type = dv;
                      fi
                  })
                  .collect(),
          )
      }

      #[test]
      fn field_infos_consistent_accepts_identical() {
          let a = fis(&[
              ("level", IndexOptions::Docs, DocValuesType::Sorted),
              ("message", IndexOptions::DocsAndFreqs, DocValuesType::None),
          ]);
          let b = fis(&[
              ("level", IndexOptions::Docs, DocValuesType::Sorted),
              ("message", IndexOptions::DocsAndFreqs, DocValuesType::None),
          ]);
          assert_field_infos_consistent(&[a, b]).unwrap();
      }

      #[test]
      fn field_infos_consistent_rejects_diverged() {
          let a = fis(&[("level", IndexOptions::Docs, DocValuesType::Sorted)]);
          // DV 类型分歧（同源写不出，手工构造）
          let b = fis(&[("level", IndexOptions::Docs, DocValuesType::None)]);
          let err = assert_field_infos_consistent(&[a, b]).unwrap_err();
          assert!(err.to_string().contains("field infos mismatch"));
          // 字段数分歧同样拒绝
          let c = fis(&[
              ("level", IndexOptions::Docs, DocValuesType::Sorted),
              ("extra", IndexOptions::None, DocValuesType::None),
          ]);
          let d = fis(&[("level", IndexOptions::Docs, DocValuesType::Sorted)]);
          assert!(assert_field_infos_consistent(&[c, d]).is_err());
      }
  ```

  跑：

  ```
  $ cargo test -p rustlucene-core merge::tests::field_infos 2>&1 | tail -5
  # 期望：2 个测试 FAILED（todo!() panic）——红
  ```

- [ ] **Step 10: 实现 field infos 一致性断言**

  `assert_field_infos_consistent` 的 todo!() 换成：

  ```rust
      let Some(first) = all.first() else { return Ok(()) };
      for (seg_idx, fis) in all.iter().enumerate().skip(1) {
          if fis.fields != first.fields {
              let names = |f: &FieldInfos| {
                  f.fields
                      .iter()
                      .map(|fi| fi.name.as_str())
                      .collect::<Vec<_>>()
                      .join(",")
              };
              return Err(io::Error::new(
                  io::ErrorKind::InvalidData,
                  format!(
                      "field infos mismatch: segment 0 [{}] vs segment {seg_idx} [{}]",
                      names(first),
                      names(fis)
                  ),
              ));
          }
      }
      Ok(())
  ```

  跑：

  ```
  $ cargo test -p rustlucene-core merge:: 2>&1 | tail -5
  # 期望：2 新测试 + Step 1-2 的 4 个全过 —— 绿
  $ cargo fmt
  $ git add crates/core/src/merge.rs crates/codec-lucene9/src/field_infos.rs
  $ git commit -m "feat: M6 T-C field infos 逐字段一致性断言（FieldInfo derive PartialEq/Eq）"
  ```

- [ ] **Step 11: postings 归并失败测试（k-way 词典归并 + 重编码 + bitmap 重建）**

  文件：`crates/core/src/merge.rs`。这是 T-C 核心。函数签名（内部）：

  ```rust
  /// postings 归并（spec §4.2 核心）：对 merged FieldInfos 里每个 indexed 字段，
  /// 各段词典 k-way 归并（TermsIter 已词典序），同 term 的文档流按段序拼接 +
  /// doc_base 偏移（天然升序，免交错——关键代码事实 3），freq 原样、positions 按
  /// doc 序拼接，走 flush 同款 PostingsWriter 重编码（FOR/PForDelta、4096 跳表
  /// 自动重建）；df/totalTermFreq 由 writer 累加；bitmap 由 with_bitmap_threshold
  /// 按归并后 df 重建。对照 SegmentMerger.mergeTerms（SegmentMerger.java:208）+
  /// MappingMultiPostingsEnum（:30；docIDShift 即 doc_base）。
  pub(crate) fn merge_postings(
      dir: &FSDirectory,
      readers: &[SegmentMergeSource],   // 本步定义，见下
      field_infos: &FieldInfos,
      new_segment: &str,
      new_segment_id: &[u8; 16],
      bitmap_threshold: Option<u32>,
  ) -> io::Result<Vec<String>> // 产出的 postings 文件名（postings.rs finish 返回）
  ```

  `SegmentMergeSource`（本步定义，后续步骤复用）：

  ```rust
  /// 一个待归并段的打开状态（按 segments_N 提交序；doc_base 已累加好）。
  pub(crate) struct SegmentMergeSource {
      pub(crate) name: String,
      pub(crate) id: [u8; 16],
      pub(crate) max_doc: i32,
      pub(crate) doc_base: u32,
      pub(crate) field_infos: FieldInfos,
  }
  ```

  失败测试设计（不用磁盘索引——直接两小段端到端）：用 `SegmentBuilder` 写两个段
  （段 0：docs 0..3，message 含 "alpha beta"、level INFO/WARN；段 1：docs 0..2，
  同词表部分重叠），`commit_segments` 提交；跑 `merge_postings` 进新段 `_m`；
  然后用 `TermsDict` + `PostingsReader` 复读新段，断言：合并 term 的 docs 带偏移
  （段 1 的 doc 0/1 → 3/4）、freq 逐 doc 保持、df 正确、singleton term 仍 singleton、
  词典序正确。bitmap 重建：threshold = 2 时 "alpha"（合并 df 5 ≥ 2）应有内联 bitmap——
  用 `PostingsReader::open_term_bitmap(entry, 5)`（postings_read.rs:175）断言
  `Some` 且 iteration 结果 == postings 枚举结果。

  ```rust
      use codec_lucene9::postings_read::PostingsReader;
      use codec_lucene9::segment_infos::{SegmentCommitInfo, SegmentInfos, random_id};
      use codec_lucene9::terms_read::TermsDict;
      use codec_lucene9::FieldInfos;

      /// 写一个小段并返回其 SegmentCommitInfo（merge 测试专用 builder）。
      fn write_segment(
          dir: &FSDirectory,
          name_counter: u64,
          docs: &[(&str, &str)], // (level, message)
      ) -> SegmentCommitInfo {
          let mut schema = Schema::new();
          schema.add(FieldSpec::keyword("level"));
          schema.add(FieldSpec::text("message"));
          let mut b = crate::SegmentBuilder::new(dir.clone(), name_counter);
          for (level, msg) in docs {
              let mut d = Document::new();
              d.add("level", FieldValue::Keyword(level.to_string()));
              d.add("message", FieldValue::Text(msg.to_string()));
              b.add_document(&schema, d).unwrap();
          }
          b.finalize().unwrap().expect("non-empty segment")
      }

      fn open_sources(dir: &FSDirectory) -> Vec<SegmentMergeSource> {
          let (infos, _gen) = SegmentInfos::read_latest(dir).unwrap();
          let mut doc_base = 0u32;
          infos
              .segments
              .iter()
              .map(|sci| {
                  let s = SegmentMergeSource {
                      name: sci.info.name.clone(),
                      id: sci.info.id,
                      max_doc: sci.info.doc_count,
                      doc_base,
                      field_infos: FieldInfos::read(dir, &sci.info.name, &sci.info.id, "").unwrap(),
                  };
                  doc_base += sci.info.doc_count as u32;
                  s
              })
              .collect()
      }

      #[test]
      fn merge_postings_offsets_and_reencodes() {
          let root = temp_dir("pmerge");
          let dir = FSDirectory::open(&root).unwrap();
          let s0 = write_segment(&dir, 0, &[
              ("INFO", "alpha beta"),
              ("WARN", "alpha"),
              ("INFO", "beta gamma"),
          ]);
          let s1 = write_segment(&dir, 1, &[("INFO", "alpha delta"), ("WARN", "alpha")]);
          let mut infos = SegmentInfos::new();
          infos.segments = vec![s0, s1];
          infos.counter = 2;
          infos.min_segment_version = Some((9, 12, 3));
          infos.commit(&dir, 1).unwrap();

          let sources = open_sources(&dir);
          assert_eq!(sources[1].doc_base, 3);
          let merged_fis = FieldInfos::new(sources[0].field_infos.fields.clone());
          let new_id = random_id();
          let files = merge_postings(
              &dir,
              &sources,
              &merged_fis,
              "_m",
              &new_id,
              Some(2), // bitmap threshold：alpha df=5 >= 2
          )
          .unwrap();
          assert!(files.iter().any(|f| f.ends_with(".doc")));

          // 复读新段：词典序 + 偏移后 postings（.fnm 由总装步写，本测试直接用 merged_fis）
          let mut dict = TermsDict::open(&dir, "_m", &new_id, &merged_fis).unwrap();
          let postings = PostingsReader::open(&dir, "_m", &new_id).unwrap();
          let msg = merged_fis.by_name("message").unwrap();
          let mut it = dict.terms_iter(msg);
          let mut terms = Vec::new();
          while let Some((term, entry)) = it.next().unwrap() {
              terms.push((String::from_utf8(term).unwrap(), entry));
          }
          assert_eq!(
              terms.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(),
              vec!["alpha", "beta", "delta", "gamma"]
          );
          let alpha = &terms[0].1;
          assert_eq!(alpha.doc_freq, 5);
          let mut en = postings.docs_and_freqs(alpha).unwrap();
          let mut got = Vec::new();
          loop {
              let d = en.next_doc().unwrap();
              if d == NO_MORE_DOCS {
                  break;
              }
              got.push((d, en.freq()));
          }
          // 段 0: alpha@(0,f1),(1,f1)；段 1: alpha@(0,f1),(1,f1) → 偏移后 3,4
          assert_eq!(got, vec![(0, 1), (1, 1), (3, 1), (4, 1)]);
          // "alpha" df=5 ≥ threshold 2 ⇒ bitmap 已重建且内容与 postings 一致
          let bm = postings.open_term_bitmap(alpha, 5).unwrap().expect("bitmap rebuilt");
          // FrozenBitmap 的批量迭代出口 docs_from（roaring/frozen.rs:163，M5 事实 5）
          let mut buf = [0u32; 8];
          let n = bm.docs_from(0, &mut buf);
          assert_eq!(&buf[..n], [0, 1, 3, 4]);
          assert_eq!(bm.cardinality(), 5);
          // singleton：gamma df=1
          let gamma = &terms[3].1;
          assert_eq!(gamma.doc_freq, 1);
          let mut en = postings.docs_and_freqs(gamma).unwrap();
          assert_eq!(en.next_doc().unwrap(), 2);
          fs::remove_dir_all(&root).unwrap();
      }
  ```

  跑：

  ```
  $ cargo test -p rustlucene-core merge::tests::merge_postings_offsets_and_reencodes 2>&1 | tail -5
  # 期望：编译失败（merge_postings / SegmentMergeSource 不存在）——红
  ```

- [ ] **Step 12: 实现 postings 归并**

  ```rust
  pub(crate) fn merge_postings(
      dir: &FSDirectory,
      sources: &[SegmentMergeSource],
      field_infos: &FieldInfos,
      new_segment: &str,
      new_segment_id: &[u8; 16],
      bitmap_threshold: Option<u32>,
  ) -> io::Result<Vec<String>> {
      use codec_lucene9::postings::PostingsWriter;
      use codec_lucene9::postings_read::PostingsReader;
      use codec_lucene9::terms_read::{TermEntry, TermsDict, TermsIter};

      let indexed: Vec<&FieldInfo> = field_infos
          .fields
          .iter()
          .filter(|f| f.index_options != IndexOptions::None)
          .collect();
      if indexed.is_empty() {
          return Ok(Vec::new()); // 无 indexed 字段：无 postings 文件（同 flush）
      }
      // 每段惰性打开 TermsDict + PostingsReader：全段无 indexed terms ⇒ flush
      // 不写 .doc/.tim（segment_builder.rs:156-161）⇒ 该段两个 reader 都是 None。
      // 判据用 .doc 文件存在性（.tmd 与 .doc 同生共死，postings.rs:290-296）。
      // `postings.rs:45` 的 `pub(crate) fn file_name` 随之改 `pub`（一行，比
      // merge.rs 里拼 "{seg}_Lucene912_0.doc" 字符串模板可靠）。
      let mut dicts: Vec<Option<TermsDict>> = Vec::with_capacity(sources.len());
      let mut readers: Vec<Option<PostingsReader>> = Vec::with_capacity(sources.len());
      for s in sources {
          if dir.file_exists(&codec_lucene9::postings::file_name(&s.name, "doc")) {
              dicts.push(Some(TermsDict::open(dir, &s.name, &s.id, &s.field_infos)?));
              readers.push(Some(PostingsReader::open(dir, &s.name, &s.id)?));
          } else {
              dicts.push(None);
              readers.push(None);
          }
      }
      let mut pw = PostingsWriter::new(dir, new_segment, new_segment_id)?
          .with_bitmap_threshold(bitmap_threshold);

      for fi in indexed {
          // doc_count = 各段 .tmd FieldTermsMeta.doc_count 之和（字段在段内无
          // terms ⇒ None ⇒ 0；write 侧 start_field 的 doc_count 同义，
          // postings.rs:308 只写进 .tmd 记录）
          let doc_count: u32 = dicts
              .iter()
              .map(|d| {
                  d.as_ref()
                      .and_then(|d| d.field_meta(fi.number))
                      .map_or(0, |m| m.doc_count as u32)
              })
              .sum();
          pw.start_field(fi, doc_count)?;

          // 每段 TermsIter peeked k-way 归并（TermsIter::next 词典序，terms_read.rs:769；
          // 字段在某段无 terms ⇒ 该段 field_meta 为 None ⇒ TermsIter 立即 done）
          let mut iters: Vec<Option<TermsIter>> = dicts
              .iter_mut()
              .map(|d| d.as_mut().map(|d| d.terms_iter(fi)))
              .collect();
          let mut heads: Vec<Option<(Vec<u8>, TermEntry)>> = Vec::with_capacity(iters.len());
          for it in iters.iter_mut() {
              heads.push(match it {
                  Some(it) => it.next()?,
                  None => None,
              });
          }
          loop {
              // 当前最小 term
              let mut min: Option<&[u8]> = None;
              for h in heads.iter().flatten() {
                  min = Some(match min {
                      None => h.0.as_slice(),
                      Some(m) if h.0.as_slice() < m => h.0.as_slice(),
                      Some(m) => m,
                  });
              }
              let Some(min_term) = min else { break };
              let min_term = min_term.to_vec();

              // 逐段拼接文档流（段序即 doc_base 序 ⇒ 全局升序，免交错）
              let mut docs: Vec<u32> = Vec::new();
              let mut freqs: Vec<u32> = Vec::new();
              let mut positions: Option<Vec<Vec<u32>>> = if has_positions(fi) {
                  Some(Vec::new())
              } else {
                  None
              };
              for (i, h) in heads.iter_mut().enumerate() {
                  let Some((term, entry)) = h else { continue };
                  if term.as_slice() != min_term.as_slice() {
                      continue;
                  }
                  let base = sources[i].doc_base;
                  let reader = readers[i].as_ref().expect("dict present ⇒ reader present");
                  match fi.index_options {
                      IndexOptions::Docs => {
                          let mut en = reader.docs(entry)?;
                          loop {
                              let d = en.next_doc()?;
                              if d == NO_MORE_DOCS { break; }
                              docs.push(base + d as u32);
                              freqs.push(1); // DOCS 字段 freq 恒 1（ttf 不入盘）
                          }
                      }
                      IndexOptions::DocsAndFreqs => {
                          let mut en = reader.docs_and_freqs(entry)?;
                          loop {
                              let d = en.next_doc()?;
                              if d == NO_MORE_DOCS { break; }
                              docs.push(base + d as u32);
                              freqs.push(en.freq());
                          }
                      }
                      _ => {
                          // DocsAndFreqsAndPositions(+Offsets)：EverythingEnum
                          let mut en = reader.positions(entry)?;
                          let pos_lists = positions.as_mut().unwrap();
                          loop {
                              let d = en.next_doc()?;
                              if d == NO_MORE_DOCS { break; }
                              docs.push(base + d as u32);
                              let f = en.freq();
                              freqs.push(f);
                              let mut plist = Vec::with_capacity(f as usize);
                              for _ in 0..f {
                                  plist.push(en.next_position()?);
                              }
                              pos_lists.push(plist);
                          }
                      }
                  }
                  *h = match &mut iters[i] {
                      Some(it) => it.next()?,
                      None => None,
                  };
              }
              debug_assert!(docs.windows(2).all(|w| w[0] < w[1]), "concatenated ascending");
              pw.write_term(&min_term, &docs, &freqs, positions.as_deref())?;
          }
          pw.finish_field()?;
      }
      pw.finish()
  }
  ```

  `has_positions`（merge.rs 内私有）：`matches!(fi.index_options,
  IndexOptions::DocsAndFreqsAndPositions | IndexOptions::DocsAndFreqsAndPositionsAndOffsets)`——
  与 postings.rs:310-315 同款。

  注意点（实现时必须照办）：
  - `use codec_lucene9::postings_read::NO_MORE_DOCS;` 加到 merge.rs 头部 import。
  - `positions` 仅在**字段级**有 positions 时构造；`.pos` 文件由 writer 首个
    positions 字段惰性创建（postings.rs:316-330），与 flush 一致。
  - `postings.rs:45` 的 `pub(crate) fn file_name` 改 `pub`（惰性 open 的
    file_exists 判据用；TermsDict/PostingsReader 的 open 内部也走它，行为不变）。

  跑：

  ```
  $ cargo test -p rustlucene-core merge:: 2>&1 | tail -5
  # 期望：全过 —— 绿
  $ cargo test -p rustlucene-core 2>&1 | tail -3   # 无回归
  $ cargo fmt
  $ git add crates/core/src/merge.rs crates/codec-lucene9/src/postings.rs
  $ git commit -m "feat: M6 T-C postings 归并——词典 k-way + doc_base 偏移拼接 + flush 同款重编码 + bitmap 重建"
  ```

- [ ] **Step 13: stored / NumericDV / SortedDV / points 归并函数失败测试**

  文件：`crates/core/src/merge.rs`。四个函数一并写失败测试（每个都是小端到端：
  两段写出 → 归并 → 复读断言）。函数签名：

  ```rust
  /// stored 块级裸拷贝（spec §4.2 修正后方案；Lucene90CompressingStoredFieldsWriter
  /// .copyChunks :520-595 主路径——同 codec、无 delete ⇒ 恒可裸拷）。
  pub(crate) fn merge_stored(
      dir: &FSDirectory,
      sources: &[SegmentMergeSource],
      new_segment: &str,
      new_segment_id: &[u8; 16],
      total_max_doc: i32,
  ) -> io::Result<[String; 3]> // [_X.fdt, _X.fdx, _X.fdm]

  /// NumericDV：顺序读 + base 重映射 + 现有 writer 重写（spec §4.2）。
  /// SortedDV：字典读 + merge_sorted_dicts 全局归并 + build_ord_remap 重映射 +
  /// 逐 doc 重写 ord（spec §4.2/§5.1）。返回 [_N_Lucene90_0.{dvd,dvm}] 或空。
  pub(crate) fn merge_doc_values(
      dir: &FSDirectory,
      sources: &[SegmentMergeSource],
      field_infos: &FieldInfos,
      new_segment: &str,
      new_segment_id: &[u8; 16],
      total_max_doc: u32,
  ) -> io::Result<Vec<String>>

  /// points：T-B BKD 读路径全区间（i64::MIN..=i64::MAX）全量遍历 +
  /// base 重映射，灌回现有 BKD writer（全内存排序吸收多段输入；
  /// PointsWriter.mergeOneField 同款朴素归并，PointsWriter.java:42）。
  pub(crate) fn merge_points(
      dir: &FSDirectory,
      sources: &[SegmentMergeSource],
      field_infos: &FieldInfos,
      new_segment: &str,
      new_segment_id: &[u8; 16],
  ) -> io::Result<Vec<String>>
  ```

  失败测试（merge.rs `mod tests` 追加，一个测试覆盖四个函数）：

  ```rust
      /// 两段含 stored + NumericDV + SortedDV + LongPoint 字段，逐格式归并后复读。
      #[test]
      fn merge_stored_dv_points_end_to_end() {
          let root = temp_dir("fmtmerge");
          let dir = FSDirectory::open(&root).unwrap();
          // 段 0：3 docs；段 1：2 docs（含 level 字典跨段重复 "INFO"）
          let schema = |s: &mut Schema| {
              s.add(FieldSpec::long_point("ts").with_numeric_dv().with_stored(true));
              s.add(FieldSpec::keyword("level").with_sorted_dv());
          };
          let mut sch = Schema::new();
          schema(&mut sch);
          let mk = |ts: i64, level: &str| {
              let mut d = Document::new();
              d.add("ts", FieldValue::Long(ts));
              d.add("level", FieldValue::Keyword(level.to_string()));
              d
          };
          let mut b0 = crate::SegmentBuilder::new(dir.clone(), 0);
          b0.add_document(&sch, mk(100, "INFO")).unwrap();
          b0.add_document(&sch, mk(200, "WARN")).unwrap();
          b0.add_document(&sch, mk(300, "INFO")).unwrap();
          let s0 = b0.finalize().unwrap().unwrap();
          let mut b1 = crate::SegmentBuilder::new(dir.clone(), 1);
          b1.add_document(&sch, mk(150, "ERROR")).unwrap();
          b1.add_document(&sch, mk(250, "INFO")).unwrap();
          let s1 = b1.finalize().unwrap().unwrap();
          let mut infos = SegmentInfos::new();
          infos.segments = vec![s0, s1];
          infos.counter = 2;
          infos.min_segment_version = Some((9, 12, 3));
          infos.commit(&dir, 1).unwrap();

          let sources = open_sources(&dir);
          let merged_fis = FieldInfos::new(sources[0].field_infos.fields.clone());
          let new_id = random_id();

          // --- stored 裸拷贝 ---
          let stored_files = merge_stored(&dir, &sources, "_m", &new_id, 5).unwrap();
          assert_eq!(stored_files, stored_fields_file_names("_m"));
          let idx = codec_lucene9::stored_fields::StoredFieldsIndexReader::open(&dir, "_m", &new_id).unwrap();
          assert_eq!(idx.num_chunks(), 2); // 每源段 1 chunk（小文档）
          assert_eq!(idx.chunk_doc_count(0), 3);
          assert_eq!(idx.chunk_doc_count(1), 2);
          // chunk 1 的 docBase 重定基为 3
          let mut fdt = dir.open_input("_m.fdt").unwrap();
          let (s, _e) = idx.chunk_byte_range(1);
          fdt.seek(s).unwrap();
          assert_eq!(fdt.read_vint().unwrap(), 3, "chunk 1 rebased to doc_base 3");

          // --- DV 归并 ---
          let dv_files = merge_doc_values(&dir, &sources, &merged_fis, "_m", &new_id, 5).unwrap();
          assert_eq!(dv_files.len(), 2);
          let dvr = codec_lucene9::doc_values_read::DocValuesReader::open(&dir, "_m", &new_id, "Lucene90_0").unwrap();
          let ts_fi = merged_fis.by_name("ts").unwrap();
          assert_eq!(
              dvr.numeric_values(ts_fi.number).unwrap(),
              vec![(0, 100), (1, 200), (2, 300), (3, 150), (4, 250)]
          );
          let level_fi = merged_fis.by_name("level").unwrap();
          let dict = dvr.sorted_dict(level_fi.number).unwrap();
          assert_eq!(
              dict.iter().map(|t| String::from_utf8(t.clone()).unwrap()).collect::<Vec<_>>(),
              vec!["ERROR", "INFO", "WARN"] // 全局字典：跨段 "INFO" 去重
          );
          // ords：ERROR=0 INFO=1 WARN=2；段 0 INFO,WARN,INFO → 1,2,1；段 1 ERROR,INFO → 0,1
          assert_eq!(
              dvr.sorted_ords(level_fi.number).unwrap(),
              vec![(0, 1), (1, 2), (2, 1), (3, 0), (4, 1)]
          );

          // --- points 归并（T-B PointsReader 全区间取点 + base 偏移 + 重写）---
          let point_files = merge_points(&dir, &sources, &merged_fis, "_m", &new_id).unwrap();
          assert_eq!(point_files.len(), 3);
          // 复读：T-B PointsReader 全区间收集 → (value, doc) 多重集与输入一致
          let pr = codec_lucene9::points_read::PointsReader::open(&dir, "_m", &new_id, &merged_fis)
              .unwrap()
              .expect("points present");
          let mut got: Vec<(i64, i32)> = Vec::new();
          pr.intersect("ts", i64::MIN, i64::MAX, &mut |v, d| got.push((v, d))).unwrap();
          got.sort();
          assert_eq!(
              got,
              vec![(100, 0), (150, 3), (200, 1), (250, 4), (300, 2)]
          );
          fs::remove_dir_all(&root).unwrap();
      }

      fn stored_fields_file_names(segment: &str) -> [String; 3] {
          [
              format!("{segment}.fdt"),
              format!("{segment}.fdx"),
              format!("{segment}.fdm"),
          ]
      }
  ```

  跑：

  ```
  $ cargo test -p rustlucene-core merge::tests::merge_stored_dv_points_end_to_end 2>&1 | tail -5
  # 期望：编译失败（四个归并函数不存在；points_read 也可能还是 T-B 在制品——
  # 若 T-B 未合入，本步阻塞等待，Global Constraints 已钉死顺序）——红
  ```

- [ ] **Step 14: 实现 stored / DV / points 归并函数**

  ```rust
  pub(crate) fn merge_stored(
      dir: &FSDirectory,
      sources: &[SegmentMergeSource],
      new_segment: &str,
      new_segment_id: &[u8; 16],
      total_max_doc: i32,
  ) -> io::Result<[String; 3]> {
      use codec_lucene9::stored_fields::{StoredFieldsIndexReader, StoredFieldsWriter};
      let mut w = StoredFieldsWriter::new(dir, new_segment, *new_segment_id, "")?;
      for s in sources {
          let idx = StoredFieldsIndexReader::open(dir, &s.name, &s.id)?;
          let [fdt_name, _fdx, _fdm] = codec_lucene9::stored_fields::file_names(&s.name, "");
          let mut fdt = dir.open_input(&fdt_name)?;
          let mut expect_base = 0i32;
          for c in 0..idx.num_chunks() {
              let (start, end) = idx.chunk_byte_range(c);
              fdt.seek(start)?;
              let src_base = fdt.read_vint()?;
              let code = fdt.read_vint()?;
              // copyChunks :558-562 的完好性断言（chunk header base == 期望 doc 序）
              if src_base != expect_base {
                  return Err(io::Error::new(
                      io::ErrorKind::InvalidData,
                      format!(
                          "corrupt fdt: segment {} chunk {c} base {src_base} != {expect_base}",
                          s.name
                      ),
                  ));
              }
              let mut payload = vec![0u8; (end - fdt.file_pointer()) as usize];
              fdt.read_bytes(&mut payload)?;
              w.append_raw_chunk(idx.chunk_doc_count(c), code, &payload)?;
              expect_base += idx.chunk_doc_count(c);
          }
      }
      let stats = w.finish(total_max_doc, dir)?;
      Ok([stats.fdt_name, stats.fdx_name, stats.fdm_name])
  }

  pub(crate) fn merge_doc_values(
      dir: &FSDirectory,
      sources: &[SegmentMergeSource],
      field_infos: &FieldInfos,
      new_segment: &str,
      new_segment_id: &[u8; 16],
      total_max_doc: u32,
  ) -> io::Result<Vec<String>> {
      use codec_lucene9::doc_values::DocValuesWriter;
      use codec_lucene9::doc_values_read::DocValuesReader;
      const DV_SUFFIX: &str = "Lucene90_0"; // segment_builder.rs:30
      let dv_fields: Vec<&FieldInfo> = field_infos
          .fields
          .iter()
          .filter(|f| f.doc_values_type != DocValuesType::None)
          .collect();
      if dv_fields.is_empty() {
          return Ok(Vec::new());
      }
      let readers: Vec<DocValuesReader> = sources
          .iter()
          .map(|s| DocValuesReader::open(dir, &s.name, &s.id, DV_SUFFIX))
          .collect::<io::Result<_>>()?;
      let mut w = DocValuesWriter::new(dir, new_segment, new_segment_id, DV_SUFFIX)?;
      for fi in dv_fields {
          match fi.doc_values_type {
              DocValuesType::Numeric => {
                  let mut pairs: Vec<(u32, i64)> = Vec::new();
                  for (s, r) in sources.iter().zip(&readers) {
                      for (d, v) in r.numeric_values(fi.number)? {
                          pairs.push((s.doc_base + d, v));
                      }
                  }
                  w.add_numeric_field(fi.number, total_max_doc, &pairs)?;
              }
              DocValuesType::Sorted => {
                  // ① 各段字典 → 全局字典 + 重映射（Step 1-2 纯函数）
                  let dicts: Vec<Vec<Vec<u8>>> = readers
                      .iter()
                      .map(|r| r.sorted_dict(fi.number))
                      .collect::<io::Result<_>>()?;
                  let global = merge_sorted_dicts(&dicts);
                  let remap = build_ord_remap(&dicts, &global);
                  // ② 逐 doc 重写 ord（doc 序 = 段序拼接，升序天然保持）
                  let mut ords: Vec<(u32, u32)> = Vec::new();
                  for (i, r) in readers.iter().enumerate() {
                      for (d, o) in r.sorted_ords(fi.number)? {
                          ords.push((sources[i].doc_base + d, remap[i][o as usize]));
                      }
                  }
                  let dict_refs: Vec<&[u8]> = global.iter().map(Vec::as_slice).collect();
                  w.add_sorted_field(fi.number, total_max_doc, &dict_refs, &ords)?;
              }
              t => {
                  return Err(io::Error::new(
                      io::ErrorKind::InvalidData,
                      format!("unsupported DV type {t:?} in merge (spec §1 premise)"),
                  ))
              }
          }
      }
      w.finish()
  }

  pub(crate) fn merge_points(
      dir: &FSDirectory,
      sources: &[SegmentMergeSource],
      field_infos: &FieldInfos,
      new_segment: &str,
      new_segment_id: &[u8; 16],
  ) -> io::Result<Vec<String>> {
      use codec_lucene9::points::PointsWriter;
      use codec_lucene9::points_read::PointsReader;
      let point_fields: Vec<&FieldInfo> = field_infos
          .fields
          .iter()
          .filter(|f| f.point_dimension_count == 1)
          .collect();
      if point_fields.is_empty() {
          return Ok(Vec::new());
      }
      let readers: Vec<Option<PointsReader>> = sources
          .iter()
          .map(|s| PointsReader::open(dir, &s.name, &s.id, &s.field_infos))
          .collect::<io::Result<_>>()?;
      let mut w = PointsWriter::new(dir, new_segment, new_segment_id)?;
      for fi in point_fields {
          // 全区间取点（spec §4.2；PointsWriter.mergeOneField 朴素归并，
          // PointsWriter.java:42-216 的 visitDocValues 路径——本系统无 delete，
          // docMap 恒等偏移）
          let mut longs: Vec<(i64, u32)> = Vec::new();
          for (i, r) in readers.iter().enumerate() {
              let Some(r) = r else { continue };
              let base = sources[i].doc_base;
              r.intersect(&fi.name, i64::MIN, i64::MAX, &mut |v, d| {
                  longs.push((v, base + d as u32));
              })?;
          }
          if longs.is_empty() {
              continue; // 全段无点：同 flush 的 field_has_points 判定
          }
          if fi.point_num_bytes == 8 {
              w.write_field_long(fi.number, &mut longs)?;
          } else {
              let mut ints: Vec<(i32, u32)> =
                  longs.iter().map(|&(v, d)| (v as i32, d)).collect();
              w.write_field_int(fi.number, &mut ints)?;
          }
      }
      w.finish()
  }
  ```

  跑：

  ```
  $ cargo test -p rustlucene-core merge:: 2>&1 | tail -5
  # 期望：全过 —— 绿
  $ cargo fmt
  $ git add crates/core/src/merge.rs
  $ git commit -m "feat: M6 T-C stored 裸拷贝 + NumericDV/SortedDV 重映射 + points 全量灌回 归并函数"
  ```

- [ ] **Step 15: `force_merge` 总装失败测试（含失败清理、旧文件删除、单段退化）**

  文件：`crates/core/src/merge.rs`。三个测试：

  ```rust
      /// 全段归并 → Searcher 侧：segment_count()==1、maxDoc 不变、查询结果与
      /// 归并前逐条一致、旧段文件与旧 segments_N 已删。
      #[test]
      fn force_merge_end_to_end() {
          let root = temp_dir("fm");
          let dir = FSDirectory::open(&root).unwrap();
          let mut schema = Schema::new();
          schema.add(FieldSpec::keyword("level"));
          schema.add(FieldSpec::text_with_positions("message"));
          schema.add(FieldSpec::long_point("ts").with_numeric_dv().with_stored(true));
          schema.add(FieldSpec::sorted_dv("host"));
          // 两个 commit → 两段（段 0 docs 0..3，段 1 docs 0..2）
          let mut w = IndexWriter::create(&root, schema, IndexWriterConfig::default()).unwrap();
          let put = |w: &mut IndexWriter, i: u64, level: &str, msg: &str, host: &str| {
              let mut d = Document::new();
              d.add("level", FieldValue::Keyword(level.to_string()));
              d.add("message", FieldValue::Text(msg.to_string()));
              d.add("ts", FieldValue::Long(1000 + i as i64));
              d.add("host", FieldValue::Keyword(host.to_string()));
              w.add_document(d).unwrap();
          };
          put(&mut w, 0, "INFO", "alpha beta", "h1");
          put(&mut w, 1, "WARN", "alpha", "h2");
          put(&mut w, 2, "INFO", "beta gamma", "h1");
          w.commit().unwrap();
          put(&mut w, 3, "ERROR", "alpha delta", "h3");
          put(&mut w, 4, "INFO", "alpha beta", "h2");
          w.commit().unwrap();
          drop(w);

          // 归并前基线
          let pre = {
              let mut s = Searcher::open(&dir).unwrap();
              assert_eq!(s.segment_count(), 2);
              let mut lines = Vec::new();
              lines.push(format!("maxDoc={}", s.max_doc()));
              for (field, term) in [
                  ("level", "INFO"), ("level", "WARN"), ("level", "ERROR"),
                  ("message", "alpha"), ("message", "beta"), ("message", "delta"),
              ] {
                  let c = s.count(&Query::term(field, term)).unwrap();
                  lines.push(format!("term {field}={term} count={c}"));
              }
              let c = s.count(&Query::phrase("message", &["alpha", "beta"])).unwrap();
              lines.push(format!("phrase alpha,beta count={c}"));
              let (_, docs) = s.top_docs(&Query::MatchAll, 20).unwrap();
              lines.push(format!("matchall first20={docs:?}"));
              lines
          };
          let files_before: std::collections::BTreeSet<String> =
              dir.list_all().unwrap().into_iter().collect();

          force_merge(&dir, &IndexWriterConfig::default()).unwrap();

          // 归并后：单段、查询逐条一致
          let mut s = Searcher::open(&dir).unwrap();
          assert_eq!(s.segment_count(), 1);
          let mut post = Vec::new();
          post.push(format!("maxDoc={}", s.max_doc()));
          for (field, term) in [
              ("level", "INFO"), ("level", "WARN"), ("level", "ERROR"),
              ("message", "alpha"), ("message", "beta"), ("message", "delta"),
          ] {
              let c = s.count(&Query::term(field, term)).unwrap();
              post.push(format!("term {field}={term} count={c}"));
          }
          let c = s.count(&Query::phrase("message", &["alpha", "beta"])).unwrap();
          post.push(format!("phrase alpha,beta count={c}"));
          let (_, docs) = s.top_docs(&Query::MatchAll, 20).unwrap();
          post.push(format!("matchall first20={docs:?}"));
          assert_eq!(pre, post, "pre/post-merge query diff must be empty");

          // 旧文件清理：旧段文件（_0/_1 前缀）与旧 segments_1/segments_2 全删，
          // 只剩新段（_2 前缀，第三段名）+ segments_3
          let files_after: Vec<String> = dir.list_all().unwrap();
          for f in &files_after {
              assert!(!f.starts_with("_0") && !f.starts_with("_1"), "stale {f}");
          }
          assert!(files_after.iter().any(|f| f == "segments_3"));
          assert!(!files_before.is_empty());
          assert_eq!(
              files_after.iter().filter(|f| f.starts_with("segments_")).count(),
              1,
              "exactly one commit file: {files_after:?}"
          );
          fs::remove_dir_all(&root).unwrap();
      }

      /// 单段退化（spec §4.4）：归并 = 重打包，结果与归并前查询一致。
      #[test]
      fn force_merge_single_segment_degenerates() {
          let root = temp_dir("fm1");
          let dir = FSDirectory::open(&root).unwrap();
          let mut schema = Schema::new();
          schema.add(FieldSpec::keyword("level"));
          let mut w = IndexWriter::create(&root, schema, IndexWriterConfig::default()).unwrap();
          for i in 0..5 {
              let mut d = Document::new();
              d.add("level", FieldValue::Keyword(format!("L{}", i % 2)));
              w.add_document(d).unwrap();
          }
          w.commit().unwrap();
          drop(w);
          force_merge(&dir, &IndexWriterConfig::default()).unwrap();
          let mut s = Searcher::open(&dir).unwrap();
          assert_eq!(s.segment_count(), 1);
          assert_eq!(s.max_doc(), 5);
          assert_eq!(s.count(&Query::term("level", "L0")).unwrap(), 3);
          let files: Vec<String> = dir.list_all().unwrap();
          assert!(files.iter().all(|f| !f.starts_with("_0.")));
          assert!(files.iter().any(|f| f == "segments_2"));
          fs::remove_dir_all(&root).unwrap();
      }

      /// 中途失败清理（spec §4.1）：field infos 不一致 ⇒ Err；目录里不留
      /// 新段孤儿文件；旧提交点完好（仍 2 段可查）。
      #[test]
      fn force_merge_failure_cleans_orphans() {
          let root = temp_dir("fmfail");
          let dir = FSDirectory::open(&root).unwrap();
          // 段 0：level；段 1：level + extra（schema 动态增长 ⇒ .fnm 分歧）
          let mut w = IndexWriter::create(&root, Schema::new(), IndexWriterConfig::default()).unwrap();
          w.schema_mut().add(FieldSpec::keyword("level"));
          let mut d = Document::new();
          d.add("level", FieldValue::Keyword("INFO".into()));
          w.add_document(d).unwrap();
          w.commit().unwrap();
          w.schema_mut().add(FieldSpec::keyword("extra"));
          let mut d = Document::new();
          d.add("level", FieldValue::Keyword("WARN".into()));
          d.add("extra", FieldValue::Keyword("x".into()));
          w.add_document(d).unwrap();
          w.commit().unwrap();
          drop(w);

          let err = force_merge(&dir, &IndexWriterConfig::default()).unwrap_err();
          assert!(err.to_string().contains("field infos mismatch"), "{err}");
          // 无 _2 前缀孤儿；旧 segments_2 仍是当前提交点
          let files: Vec<String> = dir.list_all().unwrap();
          assert!(files.iter().all(|f| !f.starts_with("_2")), "orphans: {files:?}");
          let mut s = Searcher::open(&dir).unwrap();
          assert_eq!(s.segment_count(), 2);
          assert_eq!(s.max_doc(), 2);
          fs::remove_dir_all(&root).unwrap();
      }
  ```

  跑：

  ```
  $ cargo test -p rustlucene-core merge::tests::force_merge 2>&1 | tail -8
  # 期望：编译失败（force_merge 仍是 todo!()）——红
  ```

- [ ] **Step 16: 实现 `force_merge` 总装**

  `merge.rs` 的 `force_merge` 替换 todo!()；`commit_infos` 在 index_writer.rs:144
  改 `pub(crate)`（一行：`fn commit_infos` → `pub(crate) fn commit_infos`）。

  ```rust
  /// forceMerge(1)（M6 spec §4.1）：读当前 segments_N → 逐格式归并出一个新段 →
  /// 两段式提交（复用 index_writer::commit_infos 的 fsync + pending/rename 路径）→
  /// 成功后删旧段文件与全部旧 segments_N（Java on-commit 清理同款）。
  /// 中途失败：旧提交点完好；已写出的新段文件按已知文件名清单尽力清理
  /// （SegmentMerger abort 语义）。单线程。
  pub fn force_merge(dir: &FSDirectory, config: &IndexWriterConfig) -> io::Result<()> {
      use codec_lucene9::segment_infos::{random_id, SegmentCommitInfo, SegmentInfos, SEGMENTS};

      let (old_infos, old_gen) = SegmentInfos::read_latest(dir)?;
      if old_infos.segments.is_empty() {
          return Ok(()); // 空索引 no-op（关键代码事实 10）
      }

      // 归并输入（按提交序累加 doc_base）
      let mut doc_base = 0u32;
      let mut sources: Vec<SegmentMergeSource> = Vec::with_capacity(old_infos.segments.len());
      let mut all_fis: Vec<FieldInfos> = Vec::with_capacity(old_infos.segments.len());
      for sci in &old_infos.segments {
          let fis = FieldInfos::read(dir, &sci.info.name, &sci.info.id, "")?;
          sources.push(SegmentMergeSource {
              name: sci.info.name.clone(),
              id: sci.info.id,
              max_doc: sci.info.doc_count,
              doc_base,
              field_infos: FieldInfos::new(fis.fields.clone()),
          });
          all_fis.push(fis);
          doc_base += sci.info.doc_count as u32;
      }
      assert_field_infos_consistent(&all_fis)?;
      let merged_fis = FieldInfos::new(all_fis[0].fields.clone());
      let total_max_doc = doc_base;

      // 新段名 = "_" + base36(counter)；id 全新（关键代码事实 8/11）
      let new_name = format!("_{}", crate::segment_builder::to_base36(old_infos.counter as u64));
      let new_id = random_id();
      // 失败清理清单：逐格式产出即记录（spec §4.1 abort 语义）。written 留在
      // 外层作用域（闭包只 &mut 借用），失败分支与提交失败分支都要消费它。
      let mut written: Vec<String> = Vec::new();
      let result = (|| -> io::Result<SegmentCommitInfo> {
          // .fnm 先行（全部文件同一 new_id）
          let fnm = merged_fis.write(dir, &new_name, &new_id, "")?;
          written.push(fnm.clone());
          // stored → postings → DV → points（关键代码事实 9）
          let stored = merge_stored(dir, &sources, &new_name, &new_id, total_max_doc as i32)?;
          written.extend(stored.iter().cloned());
          let bitmap_threshold = config
              .bitmap
              .then_some(config.bitmap_threshold.max(codec_lucene9::roaring::BITMAP_MIN_DF));
          let postings = merge_postings(dir, &sources, &merged_fis, &new_name, &new_id, bitmap_threshold)?;
          written.extend(postings.iter().cloned());
          let dv = merge_doc_values(dir, &sources, &merged_fis, &new_name, &new_id, total_max_doc)?;
          written.extend(dv.iter().cloned());
          let points = merge_points(dir, &sources, &merged_fis, &new_name, &new_id)?;
          written.extend(points.iter().cloned());
          // .si（diagnostics 只写稳定键，关键代码事实 7）
          let mut si = SegmentInfo::new(&new_name, new_id, total_max_doc as i32);
          si.diagnostics.insert("source".into(), "merge".into());
          si.diagnostics.insert("lucene.version".into(), "9.12.3".into());
          si.diagnostics
              .insert("mergeFactor".into(), sources.len().to_string());
          si.attributes.insert(
              "Lucene90StoredFieldsFormat.mode".into(),
              "BEST_SPEED".into(),
          );
          si.files.insert(fnm);
          si.files.extend(stored);
          si.files.extend(postings);
          si.files.extend(dv);
          si.files.extend(points);
          si.files.insert(format!("{new_name}.si"));
          si.write(dir, "")?;
          written.push(format!("{new_name}.si"));
          Ok(SegmentCommitInfo::new(si, random_id()))
      })();
      let new_sci = match result {
          Ok(sci) => sci,
          Err(e) => {
              for f in &written {
                  let _ = dir.delete(f); // 尽力而为（Java abort 同款）
              }
              return Err(e);
          }
      };

      // 两段式提交（复用现有路径：fsync 段文件 → pending → rename → dir fsync）。
      // 提交失败同样清理新段文件——失败语义与归并中途一致（spec §4.1）。
      let mut new_infos = SegmentInfos::new();
      new_infos.version = old_infos.version;
      new_infos.counter = old_infos.counter + 1;
      new_infos.index_created_version_major = old_infos.index_created_version_major;
      new_infos.min_segment_version = old_infos.min_segment_version;
      new_infos.user_data = old_infos.user_data.clone();
      new_infos.segments.push(new_sci);
      if let Err(e) = crate::index_writer::commit_infos(dir, &mut new_infos, old_gen + 1) {
          for f in &written {
              let _ = dir.delete(f);
          }
          return Err(e);
      }

      // 提交成功 ⇒ 删旧段文件 + 全部旧代 segments_N（gen ≤ old_gen）
      let mut stale: Vec<String> = Vec::new();
      for sci in &old_infos.segments {
          stale.extend(sci.info.files.iter().cloned());
      }
      for name in dir.list_all()? {
          if !name.starts_with(SEGMENTS) || name == "segments.gen" {
              continue;
          }
          let Some(gen_str) = name[SEGMENTS.len()..].strip_prefix('_') else { continue };
          let Ok(g) = i64::from_str_radix(gen_str, 36) else { continue };
          if g <= old_gen {
              stale.push(name);
          }
      }
      stale.sort();
      stale.dedup();
      for f in &stale {
          dir.delete(f)?;
      }
      Ok(())
  }
  ```

  跑：

  ```
  $ cargo test -p rustlucene-core merge:: 2>&1 | tail -5
  # 期望：全过（含 force_merge_* 三个）—— 绿
  $ cargo test -p rustlucene-core 2>&1 | tail -3 && cargo test -p codec-lucene9 2>&1 | tail -3
  # 期望：两 crate 无回归 —— 绿
  $ cargo fmt
  $ git add crates/core/src/merge.rs crates/core/src/index_writer.rs
  $ git commit -m "feat: M6 T-C force_merge 总装——逐格式归并 + 两段式提交 + 旧文件清理 + 失败孤儿清理"
  ```

- [ ] **Step 17: 边界单测之一——空索引 no-op + 字段级空（全稀疏 DV / 无 points）**

  文件：`crates/core/src/merge.rs` 测试模块。

  ```rust
      /// spec §4.4：空索引 no-op（0-doc 段本系统不存在，关键代码事实 10）。
      #[test]
      fn force_merge_empty_index_noop() {
          let root = temp_dir("fmempty");
          let dir = FSDirectory::open(&root).unwrap();
          // 手工提交一个 0 段 commit（本系统唯一构造空索引的方式；
          // Searcher::open 对 0 段 commit 已有先例：search/mod.rs:222）
          let infos = SegmentInfos::new();
          infos.commit(&dir, 1).unwrap();
          force_merge(&dir, &IndexWriterConfig::default()).unwrap();
          assert_eq!(Searcher::open(&dir).unwrap().segment_count(), 0);
          fs::remove_dir_all(&root).unwrap();
      }

      /// spec §4.4：字段级空——全稀疏 DV（段 1 latency/host 全缺）+ 无 points
      /// 字段（schema 不含 points ⇒ merge_points 空返回）。注：points 字段
      /// "部分段有数据"在本系统会让 .fnm 分歧（point flags 数据依赖，
      /// segment_builder.rs:136-142）——那是 field infos 断言的拒绝场景
      /// （Step 10/15 已覆盖），不属于归并路径。
      #[test]
      fn force_merge_field_level_empties() {
          let root = temp_dir("fmsparse");
          let dir = FSDirectory::open(&root).unwrap();
          let mut schema = Schema::new();
          schema.add(FieldSpec::keyword("level"));
          schema.add(FieldSpec::text("message"));
          schema.add(FieldSpec::numeric_dv("latency"));
          schema.add(FieldSpec::sorted_dv("host"));
          let mut w = IndexWriter::create(&root, schema, IndexWriterConfig::default()).unwrap();
          // 段 0：200 docs 全字段（latency/host 有值）
          for i in 0..200u64 {
              let mut d = Document::new();
              d.add("level", FieldValue::Keyword("INFO".into()));
              d.add("message", FieldValue::Text(format!("m{}", i % 5)));
              d.add("latency", FieldValue::Long(i as i64));
              d.add("host", FieldValue::Keyword(format!("h{}", i % 3)));
              w.add_document(d).unwrap();
          }
          w.commit().unwrap();
          // 段 1：50 docs 只有 level/message（latency/host 字段级全缺）
          for i in 0..50u64 {
              let mut d = Document::new();
              d.add("level", FieldValue::Keyword("WARN".into()));
              d.add("message", FieldValue::Text(format!("m{}", i % 5)));
              w.add_document(d).unwrap();
          }
          w.commit().unwrap();
          drop(w);

          force_merge(&dir, &IndexWriterConfig::default()).unwrap();
          let mut s = Searcher::open(&dir).unwrap();
          assert_eq!(s.segment_count(), 1);
          assert_eq!(s.max_doc(), 250);
          assert_eq!(s.count(&Query::term("level", "INFO")).unwrap(), 200);
          assert_eq!(s.count(&Query::term("level", "WARN")).unwrap(), 50);
          for i in 0..5 {
              assert_eq!(
                  s.count(&Query::term("message", &format!("m{i}"))).unwrap(),
                  50
              );
          }
          // DV 层断言：latency = 段 0 的 200 条原样（docs 0..199，无偏移）
          let (infos, _gen) = SegmentInfos::read_latest(&dir).unwrap();
          let sci = &infos.segments[0];
          let dvr = codec_lucene9::doc_values_read::DocValuesReader::open(
              &dir, &sci.info.name, &sci.info.id, "Lucene90_0",
          ).unwrap();
          let fis = FieldInfos::read(&dir, &sci.info.name, &sci.info.id, "").unwrap();
          let lat = fis.by_name("latency").unwrap();
          let vals = dvr.numeric_values(lat.number).unwrap();
          assert_eq!(vals.len(), 200);
          assert_eq!(vals[42], (42, 42));
          let host = fis.by_name("host").unwrap();
          let dict = dvr.sorted_dict(host.number).unwrap();
          assert_eq!(dict, vec![b"h0".to_vec(), b"h1".to_vec(), b"h2".to_vec()]);
          fs::remove_dir_all(&root).unwrap();
      }
  ```

  跑：

  ```
  $ cargo test -p rustlucene-core merge::tests::force_merge_empty 2>&1 | tail -3
  $ cargo test -p rustlucene-core merge::tests::force_merge_field 2>&1 | tail -3
  # 期望：全过 —— 绿
  $ cargo fmt
  $ git add crates/core/src/merge.rs
  $ git commit -m "test: M6 T-C force_merge 边界——空索引 no-op / 全稀疏 DV / 无 points 字段"
  ```

- [ ] **Step 18: 边界单测之二——bitmap on·off × positions 矩阵**

  文件：`crates/core/src/merge.rs` 测试模块。bitmap 重建与 positions 拼接的
  组合矩阵（spec §4.4）：4 组各建 2 段（段 0 塞 5000 同 term docs 让 bitmap
  门真命中），归并前后逐查询计数一致。

  ```rust
      /// spec §4.4：bitmap on/off × positions 四组合。
      #[test]
      fn force_merge_bitmap_positions_matrix() {
          for (bitmap, positions) in [(false, false), (true, false), (false, true), (true, true)] {
              let root = temp_dir(&format!("fmedge-{bitmap}-{positions}"));
              let dir = FSDirectory::open(&root).unwrap();
              let mut schema = Schema::new();
              schema.add(FieldSpec::keyword("level"));
              schema.add(if positions {
                  FieldSpec::text_with_positions("message")
              } else {
                  FieldSpec::text("message")
              });
              schema.add(FieldSpec::numeric_dv("latency")); // 段 1 全缺（稀疏）
              schema.add(FieldSpec::sorted_dv("host"));     // 段 1 全缺
              let mut config = IndexWriterConfig::default();
              config.bitmap = bitmap;
              config.bitmap_threshold = 4096;
              let mut w = IndexWriter::create(&root, schema, config).unwrap();
              // 段 0：df 做厚一点让 bitmap 门真命中（重复 level=INFO ≥4096 docs）
              for i in 0..5000u64 {
                  let mut d = Document::new();
                  d.add("level", FieldValue::Keyword("INFO".into()));
                  d.add("message", FieldValue::Text(format!("m{}", i % 10)));
                  d.add("latency", FieldValue::Long(i as i64));
                  d.add("host", FieldValue::Keyword(format!("h{}", i % 3)));
                  w.add_document(d).unwrap();
              }
              w.commit().unwrap();
              // 段 1：level/message 有值，latency/host 全缺（字段级空）
              for i in 0..100u64 {
                  let mut d = Document::new();
                  d.add("level", FieldValue::Keyword("WARN".into()));
                  d.add("message", FieldValue::Text(format!("m{}", i % 10)));
                  w.add_document(d).unwrap();
              }
              w.commit().unwrap();
              drop(w);

              let count_battery = |s: &mut Searcher, positions: bool| {
                  let mut counts = Vec::new();
                  for t in ["INFO", "WARN"] {
                      counts.push(s.count(&Query::term("level", t)).unwrap());
                  }
                  for i in 0..10 {
                      counts.push(
                          s.count(&Query::term("message", &format!("m{i}"))).unwrap(),
                      );
                  }
                  if positions {
                      counts.push(s.count(&Query::phrase("message", &["m1"])).unwrap());
                  }
                  counts
              };
              let pre_counts = {
                  let mut s = Searcher::open(&dir).unwrap();
                  count_battery(&mut s, positions)
              };
              force_merge(&dir, &IndexWriterConfig {
                  bitmap,
                  bitmap_threshold: 4096,
                  ..IndexWriterConfig::default()
              })
              .unwrap();
              let mut s = Searcher::open(&dir).unwrap();
              assert_eq!(s.segment_count(), 1);
              assert_eq!(s.max_doc(), 5100);
              assert_eq!(
                  pre_counts,
                  count_battery(&mut s, positions),
                  "bitmap={bitmap} positions={positions}"
              );
              fs::remove_dir_all(&root).unwrap();
          }
      }
  ```

  跑：

  ```
  $ cargo test -p rustlucene-core merge::tests::force_merge_bitmap_positions_matrix 2>&1 | tail -3
  # 期望：全过 —— 绿
  $ cargo fmt
  $ git add crates/core/src/merge.rs
  $ git commit -m "test: M6 T-C force_merge 边界矩阵——bitmap on·off × positions"
  ```

- [ ] **Step 19: CLI `forcemerge` 子命令 + `logwrite --flush-every`**

  文件：`crates/core/src/bin/rustlucene-cli.rs`。

  ① main 的 match 加分支（放在 `"logbench"` 之后）：

  ```rust
          "forcemerge" => {
              if args.len() < 3 {
                  usage();
              }
              let mut config = IndexWriterConfig::default();
              if args[3..].iter().any(|a| a == "--bitmap") {
                  config.bitmap = true;
                  config.bitmap_threshold = args[3..]
                      .windows(2)
                      .find_map(|w| {
                          (w[0] == "--bitmap-threshold")
                              .then(|| w[1].parse::<u32>().unwrap_or_else(|_| usage()))
                      })
                      .unwrap_or(4096)
                      .max(4096); // 同 logwrite：读侧 BITMAP_MIN_DF=4096 下界 clamp
              }
              let dir = FSDirectory::open(Path::new(&args[2]))?;
              let t0 = Instant::now();
              rustlucene_core::merge::force_merge(&dir, &config)?;
              let ms = t0.elapsed().as_millis().max(1);
              println!("FORCEMERGED index={} elapsed_ms={ms}", args[2]);
              Ok(())
          }
  ```

  ② `logwrite` 支持 `--flush-every N`（电池多段输入，关键代码事实 12）：
  `logwrite(...)` 函数签名加 `flush_every: Option<u32>`，函数体开头：

  ```rust
      let mut config = IndexWriterConfig::default();
      if let Some(n) = flush_every {
          config.max_buffered_docs = n; // IndexWriter.add_document 的既有 flush 触发器
      }
      if let Some(t) = bitmap {
          config.bitmap = true;
          config.bitmap_threshold = t;
      }
  ```

  main 的 `"logwrite"` 分支解析（windows(2) 同款模式，加在 bigdict 解析后）：

  ```rust
              let flush_every = args[5..]
                  .windows(2)
                  .find_map(|w| {
                      (w[0] == "--flush-every")
                          .then(|| w[1].parse::<u32>().unwrap_or_else(|_| usage()))
                  });
              logwrite(
                  Path::new(&args[2]),
                  args[3].parse().unwrap(),
                  args[4].parse().unwrap(),
                  positions,
                  sparse,
                  bigdict,
                  bitmap,
                  flush_every,
              )
  ```

  ③ usage() 两行：

  ```rust
      eprintln!("  rustlucene-cli logwrite <indexDir> <numDocs> <seed> [--positions] [--sparse] [--bigdict] [--bitmap [--bitmap-threshold N]] [--flush-every N]");
      eprintln!("  rustlucene-cli forcemerge <indexDir> [--bitmap] [--bitmap-threshold N]");
  ```

  （logwrite 原 usage 行就地替换为带 `--flush-every` 的版本。）

  手测：

  ```
  $ cargo build --release 2>&1 | tail -2
  $ rm -rf /tmp/fm-cli && cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
      logwrite /tmp/fm-cli 20000 42 --positions --flush-every 5000
  $ ls /tmp/fm-cli | grep -c "^_.*\.si$"   # 期望：4 段（_0.._3）
  $ cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- forcemerge /tmp/fm-cli
  FORCEMERGED index=/tmp/fm-cli elapsed_ms=...
  $ cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- searchdump /tmp/fm-cli 20000 42 --positions | head -2
  maxDoc=20000
  $ ls /tmp/fm-cli | grep -c "^_.*\.si$"   # 期望：1 段（_4）
  $ ls /tmp/fm-cli | grep segments          # 期望：仅 segments_2
  $ cargo fmt
  $ git add crates/core/src/bin/rustlucene-cli.rs
  $ git commit -m "feat: M6 T-C CLI forcemerge 子命令 + logwrite --flush-every（电池多段输入）"
  ```

- [ ] **Step 20: log-test 电池新增 Rust-forceMerge 变体（spec §4.4 ①②③）**

  文件：`interop/verify-log.sh`（重构分发 + 新变体函数）、`Makefile`（加两行）。

  `verify-log.sh` 完整替换为：

  ```bash
  #!/usr/bin/env bash
  # M2 interop: Rust writes a log-schema index -> Java CheckIndex + query dump;
  # Java writes the same corpus with stock Lucene -> CheckIndex + query dump;
  # the two dumps must be identical.
  # M6 T-C: --forcemerge / --forcemerge-bitmap 走另一条流水线——logbench 8 线程
  # 产多段索引 → 归并前 searchdump 基线 → rust forcemerge → CheckIndex ① +
  # 归并前后 diff ② + 同索引 Java forceMerge(1) 交叉 diff ③（spec §4.4）。
  # Usage: interop/verify-log.sh [numDocs] [seed] [--positions|--sparse|--bigdict|--bitmap|--forcemerge|--forcemerge-bitmap]
  set -euo pipefail

  ROOT="$(cd "$(dirname "$0")/.." && pwd)"
  NUM_DOCS="${1:-200000}"
  SEED="${2:-42}"
  MODE="${3:-}"
  RUST_DIR=/tmp/rl-log-rust
  JAVA_DIR=/tmp/rl-log-java
  CP="$ROOT/interop/java/classes:$ROOT/interop/java/lib/lucene-core-9.12.3.jar:$ROOT/interop/java/lib/lucene-analysis-common-9.12.3.jar"

  run_standard_variant() {
    rm -rf "$RUST_DIR" "$JAVA_DIR"
    mkdir -p "$RUST_DIR" "$JAVA_DIR"

    echo "== Rust: logwrite ($NUM_DOCS docs, seed $SEED $MODE)"
    cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
      logwrite "$RUST_DIR" "$NUM_DOCS" "$SEED" $MODE

    echo "== Java: JavaLogBench (same corpus)"
    java -cp "$CP" JavaLogBench "$JAVA_DIR" "$NUM_DOCS" 1 "$SEED" $MODE
    for side in "$RUST_DIR" "$JAVA_DIR"; do
      echo "== CheckIndex $side"
      java -cp "$CP" org.apache.lucene.index.CheckIndex "$side" 2>&1 \
        | grep -E "No problems|FAILED|error" || true
      java -cp "$CP" org.apache.lucene.index.CheckIndex "$side" > /dev/null 2>&1
    done

    echo "== VerifyLogIndex: Rust vs Java dumps"
    EXPECT_POSITIONS=false
    [ "$MODE" = "--positions" ] && EXPECT_POSITIONS=true
    java -cp "$CP" VerifyLogIndex "$RUST_DIR" "$EXPECT_POSITIONS" > /tmp/rl-log-rust.out
    java -cp "$CP" VerifyLogIndex "$JAVA_DIR" "$EXPECT_POSITIONS" > /tmp/rl-log-java.out
    diff -u /tmp/rl-log-rust.out /tmp/rl-log-java.out
    cat /tmp/rl-log-rust.out

    echo "== Search diff: searchdump vs VerifySearchIndex"
    "$ROOT/interop/verify-search.sh" "$RUST_DIR" "$JAVA_DIR" "$NUM_DOCS" "$SEED" "$MODE"

    if [ "$MODE" = "--bitmap" ]; then
      echo "== Bitmap A/B: Rust searchdump bitmap on vs off (RL_BITMAP=0)"
      env -u RL_BITMAP cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
        searchdump "$RUST_DIR" "$NUM_DOCS" "$SEED" > /tmp/rl-search-bitmap-on.out
      RL_BITMAP=0 cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
        searchdump "$RUST_DIR" "$NUM_DOCS" "$SEED" > /tmp/rl-search-bitmap-off.out
      diff -u /tmp/rl-search-bitmap-on.out /tmp/rl-search-bitmap-off.out

      echo "== Java forceMerge on the Rust --bitmap index"
      java -cp "$CP" ForceMergeIndex "$RUST_DIR"
      echo "== CheckIndex post-merge $RUST_DIR"
      java -cp "$CP" org.apache.lucene.index.CheckIndex "$RUST_DIR" 2>&1 \
        | grep -E "No problems|FAILED|error" || true
      java -cp "$CP" org.apache.lucene.index.CheckIndex "$RUST_DIR" > /dev/null 2>&1

      echo "== Post-merge search diff (merged index carries no bitmaps)"
      "$ROOT/interop/verify-search.sh" "$RUST_DIR" "$JAVA_DIR" "$NUM_DOCS" "$SEED" ""
    fi
  }

  # M6 T-C（spec §4.4）：多段索引（logwrite --flush-every 单写线程 8 段，语料与
  # searchdump 重放一致——关键代码事实 12）→ rust forcemerge →
  # ① Java CheckIndex 零错误；② 归并前后全量查询 diff（positions 电池全量跑，
  #   bitmap 变体加 RL_BITMAP A/B）；③ 与同语料 Java forceMerge(1) 产物全文件交叉 diff。
  run_forcemerge_variant() {
    local BITMAP_FLAG=""
    local FLUSH_EVERY=$((NUM_DOCS / 8))
    if [ "$MODE" = "--forcemerge-bitmap" ]; then
      BITMAP_FLAG="--bitmap"
    fi
    rm -rf "$RUST_DIR" "$JAVA_DIR"
    mkdir -p "$RUST_DIR" "$JAVA_DIR"

    echo "== Rust: logwrite multi-segment ($NUM_DOCS docs, seed $SEED --positions --flush-every $FLUSH_EVERY $BITMAP_FLAG)"
    cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
      logwrite "$RUST_DIR" "$NUM_DOCS" "$SEED" --positions --flush-every "$FLUSH_EVERY" $BITMAP_FLAG

    echo "== Pre-merge searchdump (baseline for ②)"
    cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
      searchdump "$RUST_DIR" "$NUM_DOCS" "$SEED" --positions > /tmp/rl-fm-pre.out

    echo "== Snapshot pre-merge index for the Java cross-merge ③"
    cp -a "$RUST_DIR"/. "$JAVA_DIR"/

    echo "== Rust: forcemerge $BITMAP_FLAG"
    cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
      forcemerge "$RUST_DIR" $BITMAP_FLAG

    echo "== ① CheckIndex post-merge $RUST_DIR"
    java -cp "$CP" org.apache.lucene.index.CheckIndex "$RUST_DIR" 2>&1 \
      | grep -E "No problems|FAILED|error" || true
    java -cp "$CP" org.apache.lucene.index.CheckIndex "$RUST_DIR" > /dev/null 2>&1

    echo "== ② Post-merge searchdump vs pre-merge baseline"
    cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
      searchdump "$RUST_DIR" "$NUM_DOCS" "$SEED" --positions > /tmp/rl-fm-post.out
    diff -u /tmp/rl-fm-pre.out /tmp/rl-fm-post.out
    cat /tmp/rl-fm-post.out

    if [ "$MODE" = "--forcemerge-bitmap" ]; then
      echo "== ②b Bitmap A/B on the merged index (RL_BITMAP=0)"
      RL_BITMAP=0 cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
        searchdump "$RUST_DIR" "$NUM_DOCS" "$SEED" --positions > /tmp/rl-fm-post-off.out
      diff -u /tmp/rl-fm-post.out /tmp/rl-fm-post-off.out
    fi

    echo "== ③ Java forceMerge(1) on the same pre-merge index"
    java -cp "$CP" ForceMergeIndex "$JAVA_DIR"
    java -cp "$CP" VerifySearchIndex "$JAVA_DIR" --positions > /tmp/rl-fm-java.out
    diff -u /tmp/rl-fm-post.out /tmp/rl-fm-java.out

    echo "FORCEMERGE_INTEROP_OK"
  }

  case "$MODE" in
    --forcemerge|--forcemerge-bitmap) run_forcemerge_variant ;;
    *) run_standard_variant ;;
  esac

  echo "LOG_INTEROP_OK"
  ```

  注意（实现时逐条核对，不许想当然）：
  - `cp -a "$RUST_DIR"/. "$JAVA_DIR"/` 拷的是**归并前**多段索引；Java
    ForceMergeIndex 对其 forceMerge(1)（APPEND 模式，ForceMergeIndex.java:17-30）。
  - ③的 diff 用 Rust searchdump vs Java VerifySearchIndex 全文件直接比——两个
    dumper 行格式本来就逐行对齐（verify-search.sh 的既有比较方式）；索引是同一份
    语料（单写线程重放语料），Java 侧 trace_id/phrase 电池行从 stored 取真值
    （VerifySearchIndex.java:60-65/151-153）与 Rust 侧 seed 重放
    （rustlucene-cli.rs:734-768）指向同一内容，trace_id/doc7 等行逐字节一致。
    这是选 logwrite --flush-every 而非 logbench 8 线程的直接原因（关键代码事实 12）。
  - 本变体**不跑** VerifyLogIndex/verify-search.sh 的 Rust-vs-Java 索引构建对拍
    （那是标准变体的职责）；③ 是"同一份磁盘索引、两侧各自 forceMerge(1) 后查询
    一致"，正是 spec §4.4③ 的字面要求。
  - `--forcemerge` 变体归并后索引无 bitmap（forcemerge 不带 --bitmap ⇒ threshold
    None ⇒ 纯 postings 重编码），RL_BITMAP A/B 无意义故只在 bitmap 变体跑。
  - `$BITMAP_FLAG` 刻意不加引号（空串即无参；与脚本既有 `$MODE`/`$POSITIONS`
    用法同款），不要用数组——`set -u` 下空数组展开在老 bash 上报错。

  `Makefile` log-test 目标改为：

  ```make
  # M2 log-schema interop: Rust logwrite vs JavaLogBench, CheckIndex + query diff
  # M6 T-C: 加两个 Rust-forceMerge 变体（spec §4.4）
  log-test: build java-classes
  	interop/verify-log.sh 200000 42
  	interop/verify-log.sh 200000 43 --positions
  	interop/verify-log.sh 200000 44 --sparse
  	interop/verify-log.sh 200000 45 --bigdict
  	interop/verify-log.sh 200000 46 --bitmap
  	interop/verify-log.sh 200000 47 --forcemerge
  	interop/verify-log.sh 200000 48 --forcemerge-bitmap
  ```

  跑（电池全量，约与原五变体同量级时间）：

  ```
  $ make log-test 2>&1 | tee /tmp/log-test-tc.log | grep -E "LOG_INTEROP_OK|FORCEMERGE_INTEROP_OK|FAILED|diff|error" | head -20
  # 期望：7 个 LOG_INTEROP_OK（含 2 个 FORCEMERGE_INTEROP_OK），无 FAILED/diff 输出
  $ git add interop/verify-log.sh Makefile
  $ git commit -m "test: M6 T-C log-test 电池挂 --forcemerge/--forcemerge-bitmap 变体（CheckIndex + 前后 diff + Java 交叉 diff）"
  ```

**验收清单（spec §4.4 逐条映射）:**

- [ ] `make log-test` 七变体全绿；`--forcemerge*` 两变体输出 `FORCEMERGE_INTEROP_OK`
- [ ] ① Java CheckIndex 对 rust forceMerge 产物 "No problems"（变体脚本内）
- [ ] ② 归并前后全量查询 diff 为空（term/bool/terms/prefix/wildcard/phrase/matchall
  全电池，positions 变体；bitmap A/B 两侧在 `--forcemerge-bitmap` 变体内）
- [ ] ③ 与同语料 Java forceMerge(1) 产物查询 diff 交叉一致（变体脚本内）
- [ ] 单测边界（Step 15/17/18）：单段退化 / 空索引 no-op（0-doc 段本系统不存在，
  关键代码事实 10）/ 全稀疏 DV / 无 points / bitmap on·off / positions 字段
- [ ] 归并后 `Searcher.segment_count() == 1` 且旧文件清理断言（Step 15 测试：
  目录清单无 `_0./_1.` 前缀、仅一个 `segments_N`）
- [ ] `cargo test -p codec-lucene9` / `cargo test -p rustlucene-core` 全绿；
  `cargo fmt --check` 干净；`cargo check --workspace`（含 jni-binding）干净
---

### Task D: 收尾验收（spec §5 风险 3 + §6）

**Files:**
- Modify: `.superpowers/sdd/progress.md`（gitignored，M6 记账）

**Interfaces:**
- Consumes: T-A/T-B/T-C 全部落地后的 main；M5 bench 口径（`.superpowers/sdd/m5-bench-report.md` §7 命令清单）

- [ ] **Step 1: 全量电池**

```bash
cargo fmt --check
cargo test -p codec-lucene9
cargo test -p rustlucene-core
cargo check --workspace    # 含 jni-binding
make log-test              # 七变体（T-C 新增两个 forcemerge 变体）全绿
```

Expected: fmt 干净；测试全绿；log-test EXIT=0，CheckIndex 全部 "No problems"。

- [ ] **Step 2: 三路 bench 复跑（拍平漏判兜底，对照 M5 基线）**

照 `.superpowers/sdd/m5-bench-report.md` §7 的 Step 5.3–5.5 命令原样复跑（同语料 seed 42
1M docs、`--bitmap` 索引、同一 m5-q.txt 查询集、`--warmup 10 --iter 30`、三路串行、
`--no-cache` 口径）：

```bash
CP="interop/java/classes:interop/java/lib/lucene-core-9.12.3.jar:interop/java/lib/lucene-analysis-common-9.12.3.jar"
cargo build --release
cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- logwrite /tmp/rl-bench6-rust 1000000 42 --bitmap
java -cp "$CP" JavaLogBench /tmp/rl-bench6-java 1000000 1 42
cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- searchbench /tmp/rl-bench6-rust message \
  --load-queries .superpowers/sdd/m5-q.txt --warmup 10 --iter 30 \
  > .superpowers/sdd/m6-bench-rust-roaring.out 2> .superpowers/sdd/m6-counts-rust-roaring.txt
RL_BITMAP=0 cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- searchbench /tmp/rl-bench6-rust message \
  --load-queries .superpowers/sdd/m5-q.txt --warmup 10 --iter 30 \
  > .superpowers/sdd/m6-bench-rust-pfor.out 2> .superpowers/sdd/m6-counts-rust-pfor.txt
java -cp "$CP" SearchBench /tmp/rl-bench6-java message \
  --load-queries .superpowers/sdd/m5-q.txt --no-cache --warmup 10 --iter 30 \
  > .superpowers/sdd/m6-bench-java.out 2> .superpowers/sdd/m6-counts-java.txt
diff .superpowers/sdd/m6-counts-rust-roaring.txt .superpowers/sdd/m6-counts-rust-pfor.txt   # 期望为空
```

判定：① counts diff 为空（roaring == pfor）；② 核心四组（term/and/or/iterm high）roaring/pfor
qps 比对照 M5 基线（1.06 / 7.02 / 12.70 / 1.23）**回退不超过 15%**（运行间噪声量级之外
才算回退）；超线即查 T-A 拍平形状判定是否漏拍（spec §5 风险 3）。产物写
`.superpowers/sdd/m6-bench-report.md`（gitignored）。

- [ ] **Step 3: M6 终审 + 记账**

全分支终审（0 Critical/0 Important 才可收口），deferred Minor 记入
`.superpowers/sdd/progress.md`；README「搜索读路径」节补嵌套 Bool / PointRange / forcemerge
能力行；`git log` 核对三任务提交链完整。

- [ ] **Step 4: Commit**

```bash
git add README.md
git commit -m "docs: README 收编 M6 能力（嵌套 Bool / PointRange / forcemerge）"
```
