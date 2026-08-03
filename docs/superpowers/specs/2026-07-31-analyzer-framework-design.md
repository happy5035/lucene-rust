# Analysis 框架设计：per-field analyzer + LowerCaseFilter + terms IN

2026-07-31。目标场景：日志搜索。本设计为 `lucene-rust` 引入可扩展的
analysis 框架，首轮落地 per-field analyzer 配置、LowerCaseFilter、JNI
JSON 的 `terms`（IN）查询类型。中文分词明确不在范围内。

格式/语义对照仍以 `reference/lucene-9.12.3/` 为唯一事实来源。

## 背景与问题

当前分析链路是 MVP 状态（调研见 2026-07-31 会话）：

- `crates/core/src/tokenizer.rs` 只有一个 `WhitespaceTokens`
  （`split_ascii_whitespace` 包装），硬编码在 `doc_writer.rs` 的写入热
  路径，无抽象、不可插拔。
- 字段层面分析配置只有一个 `tokenized: bool`（`schema.rs`）。
- 查询侧完全没有分析入口：`Query::term(field, value)` 收的是调用方切好
  的原始字节。索引/查询归一化一致性靠"两侧都不做任何处理"的隐性约定
  维持，加上任何 filter 都会系统性查不到。
- 大小写敏感是日志场景最痛的缺口：搜 `error` 漏掉 `ERROR` /
  `NullPointerException`。
- 查询引擎已有 `Query::terms`（IN 语义，≤16 析取 / >16 bitset），但
  JNI JSON 查询解析（`jni-binding/src/query_parser.rs`）没有对应入口。

## 需求（已与用户确认）

1. **可扩展 analyzer 框架**：后续要基于用户配置对每个索引设置不同分词
   器，主要覆盖 Lucene `CharTokenizer` 一族；参考 Lucene 实现，性能结
   合 Rust 特性（零分配热路径、无继承体系）。
2. **LowerCaseFilter**，走 per-field analyzer 配置生效。
3. **per-field analyzer**：每个字段独立配置；Rust 实现，Java 侧只提供
   构造 analyzer 所需的参数（schema spec 字符串）。
4. **terms IN 语法**：JNI JSON 加 `terms` 类型，映射到已有
   `Query::terms`，不做独立查询字符串解析器。
5. **查询侧对齐 Lucene 双通道**：Term/Phrase/Terms 走完整 token 流，
   Prefix/Wildcard 只走 normalize。
6. 中文分词不做；不为 stop/synonym filter 提前设计。

## 架构：方案 A —— 静态 enum 链 + 名字注册表 + trait 逃生舱

与项目现有风格一致（`SegmentDocIter` 即 enum 分发而非 Java 式继承）：
内置组件编译成枚举走静态分发，热路径零虚调用；`Custom` 变体挂外部扩
展；配置就是一个字符串。

```rust
enum TokenizerKind { Whitespace, Letter, Keyword, Custom(Box<dyn Tokenizer>) }
enum FilterKind    { Lowercase, Custom(Box<dyn TokenFilter>) }

struct Analyzer { tokenizer: TokenizerKind, filters: Vec<FilterKind> }
```

被否决的候选：纯 trait object 链（逐行对齐 Lucene `Analyzer`/
`TokenStream`，每 token 虚调用 + 每文档重建链，性能过度对齐）；回调管
道（`Fn(&str, &mut dyn FnMut(&[u8]))`，filter 有状态组合和 normalize
通道难表达）。

## 组件接口与 token 流语义

新模块 `crates/core/src/analysis/`（`mod.rs` / `tokenizer.rs` /
`filter.rs` / `registry.rs`），现有顶层 `tokenizer.rs` 迁入。

```rust
/// 一词进一词出；None = 丢弃（为将来 stop filter 留口，本轮不实现）。
/// Cow 语义：无大写字符的 token 零分配原样传递。
pub trait TokenFilter: Send + Sync {
    fn filter<'a>(&mut self, token: Cow<'a, [u8]>) -> Option<Cow<'a, [u8]>>;
    /// normalize 通道是否包含本 filter（Lucene MultiTermAware 语义）。
    fn normalizes(&self) -> bool;
}

pub trait Tokenizer: Send {
    fn reset(&mut self, input: &str);
    /// pos_incr 恒 1（内置组件）；返回值借用 input，零分配。
    fn next_token(&mut self) -> Option<(Cow<'_, [u8]>, u32)>;
}
```

`Analyzer` 为编译好的链，两个入口对齐 Lucene 双通道：

- `analyze(&mut self, input) -> impl Iterator<Item = (Cow<[u8]>, u32)>`
  —— 完整 token 流（索引、Term/Phrase/Terms 查询用）。
- `normalize(&self, input) -> Cow<str>` —— 不切词，整串过一遍
  `normalizes() == true` 的 filter（Prefix/Wildcard 用；lowercase 属
  于此类，对齐 Lucene `LowerCaseFilter` 的 normalize 行为）。

内置组件（首轮）：

| 组件 | 类型 | 对照 Lucene | 说明 |
|---|---|---|---|
| `whitespace` | tokenizer | `WhitespaceTokenizer` | 现实现平移，ASCII 空白切分 |
| `letter` | tokenizer | `LetterTokenizer` | `char::is_alphabetic` 切分；与 Java `Character.isLetter` 的 Unicode 版本差异在注释钉死 |
| `keyword` | tokenizer | `KeywordTokenizer` | 整串一词 |
| `lowercase` | filter | `LowerCaseFilter` | ASCII 快路径零分配；含非 ASCII 大写走 `to_lowercase()`，Unicode 语义对齐 Lucene；`normalizes() = true` |

最小语义约定：filter 一词进一词出，pos_incr 恒 1——不为将来的
stop/synonym 提前设计。

## 索引侧接入

- `FieldSpec` 增加 `analyzer: Option<String>`（规格文本，如
  `"whitespace|lowercase"`），仅在 `tokenized == true` 且已索引的
  text 字段上合法；`Schema::add` assert，挂在 keyword/非索引字段上报
  错（对齐 Lucene：StringField 不分析）。
- **编译时机**：`DocWriter`（含每个 `SegmentBuilder` 分片）构建时按字
  段把 analyzer 规格编译成 `Analyzer` 实例，存在 per-field buffer 旁，
  `add` 时 reset 复用——每字段每 writer 只编译一次，热路径无注册表
  查找。
- `doc_writer.rs` 写入热路径：有 analyzer 走 `analyze()` 流；**无
  analyzer 字段保持现状逐字节不动**——所有既有索引、interop 对拍
  （`make log-test` 11 次 CheckIndex + 查询 diff）零影响。`text` 不
  配 analyzer 时行为 = 今天的 whitespace，是否内部统一走 `Whitespace`
  组件只是实现细节，对外字节一致。
- 内存搜索路径（`MemoryLeafAccess`）天然一致：内存 postings 存的就是
  分析后的 token，索引/查询共用同一分析结果。
- RAM 记账不变：token 是 Cow 借用；lowercase 触发分配仅为临时分配，
  不进词典常驻。

## 查询侧双通道

analyzer 配置存在 writer 的 schema 里，**不在索引格式里**（Lucene 同
样如此——`.fnm` 不记 analyzer，分析是应用层职责）。分析动作放在查询
构建层，执行引擎（`Query` / postings）完全不感知 analyzer。

- 新函数 `analysis::analyze_query(q: &Query, specs: &FieldAnalyzers)
  -> Result<Query, String>`：输入已解析的 Query + 字段→预解析
  analyzer 规格表，输出重写后的 Query。JNI `nativeSearch` 在
  `spec_to_query` 之后调用。
- **锁问题**：`IndexWriter::search` 持读锁，拿不到 `&mut Analyzer`。
  查询侧**每查询现组装组件**：`analyze_query` 的 `analyzer_for` 每查
  询对规格文本调 `Analyzer::parse` 全量重解析——new 一组无状态组
  件（含一次注册表读锁 + HashMap 查找，成本与预解析方案同阶），对
  <100 QPS 目标无感。
- 重写规则：

  | 查询类型 | 通道 | 规则 |
  |---|---|---|
  | Term | analyze | 0 token → `Err`（显式报错好于静默空结果）；1 token → 替换；多 token → Bool SHOULD 析取（对齐 Lucene QueryParser 默认 OR） |
  | Terms（IN） | analyze | 每个 value 独立分析，必须恰好 1 token 否则 `Err` |
  | Phrase | analyze | JNI API 是 `terms: Vec<String>`，每项独立分析，必须恰好 1 token 否则 `Err`（slop=0 精确短语下的正确语义） |
  | Prefix / Wildcard | normalize | 只归一化，不切词 |
  | Range / MatchAll | — | 不动 |
  | Bool | — | 递归重写子句 |

- CLI 的 sexpr / 磁盘 Searcher 路径**不做**分析（无 schema 上下文），
  保持原样并在文档注明：磁盘直查时调用方负责自行归一化——与 Lucene
  「analyzer 是应用层概念」一致。

## 配置语法（schema spec）

```
message:text+positions+analyzer=whitespace|lowercase,level:keyword+sorteddv,...
```

- `analyzer=` 是一个 modifier，值内组件用 `|` 分隔（`+` 是 modifier
  分隔符、`,` 是字段分隔符、`:`/`@` 已占用，`|` 无冲突）。第一个组
  件必须是 tokenizer，其余为 filter，顺序即链序。
- 名字注册表内置 `whitespace` / `letter` / `keyword` / `lowercase`；
  外部扩展 `AnalyzerRegistry::register_tokenizer(name, factory)` /
  `register_filter(...)`，同一张表，`Custom` 变体承接。
- 校验 fail fast（建索引前报错，Java 侧第一时间拿到明确信息）：未知
  组件名、analyzer 挂在 keyword/非索引字段、filter 出现在链首。
- `FieldSpec::text(...)` / `text_with_positions(...)` 增加链式
  `with_analyzer("whitespace|lowercase")`，Rust 调用方不走字符串。

## JNI terms IN

```json
{"query":{"type":"terms","field":"level","values":["ERROR","WARN"]}}
```

- `QuerySpec` 加 `Terms { field, values: Vec<String> }` 变体 → 分析
  重写 → 现有 `Query::terms`（引擎零改动：≤16 析取 / >16 bitset、
  count 快路径、roaring 折叠全部自动继承）。
- Bool 子句里可任意嵌套。

## 测试与兼容验证

- **组件单测**（`analysis/` 内）：whitespace/letter/keyword 与 Lucene
  对应类 golden case 对齐；lowercase 的 ASCII 零分配路径 + 非 ASCII
  回退路径；`analyze`/`normalize` 双通道各自行为。
- **索引侧**：带 analyzer 字段写入 → 词典内容断言（`ERROR error
  Error` 归一到同一 term）；默认无 analyzer 路径与现状逐字节一致
  （现有 core 128 项测试不改即全绿）。
- **查询侧**：`analyze_query` 全规则单测（Term 多 token 转 SHOULD、
  terms/phrase 单 token 约束报错、prefix/wildcard normalize、Bool 递
  归、无 analyzer 字段原样透传）。
- **端到端**：JNI JSON `terms` 查询 + lowercase 字段写入-搜索闭环
  （core 层 `IndexWriter::search` 覆盖，不起 JVM）。
- **互操作回归（硬约束）**：`make log-test` 全绿——默认路径行为不变。
- **性能回归**：`make log-bench` 1M docs 写入吞吐对比（预期持平：无
  analyzer 字段走原路径；whitespace 组件与 `SplitAsciiWhitespace` 同
  构）。带 lowercase 的吞吐记录基线即可，不设门槛。
- **README**：更新「字段类型」「范围与限制」，补 analyzer 配置语法。

## 明确不做

- 中文 / CJK 分词、smartcn、ngram。
- stop / synonym filter（接口留了 `Option` 丢弃口和 pos_incr 字段，不
  提前实现）。
- 独立查询字符串解析器（query_string 语法）。
- 磁盘 Searcher / CLI 路径的查询侧分析。
- 索引格式变更：analyzer 配置不落盘，与 Lucene 一致。
