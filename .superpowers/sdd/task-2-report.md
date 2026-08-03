# Task 2 Report: LowercaseFilter + Analyzer 模板与 TokenStream

## What was implemented

严格按 brief 逐字实现，无 drift。

### `crates/core/src/analysis/filter.rs`（新建）
- `pub trait TokenFilter: Send + Sync` — `filter<'a>(&self, Cow<'a, [u8]>) -> Option<Cow<'a, [u8]>>` + `normalizes(&self) -> bool`
- `pub struct LowercaseFilter` — ASCII 快路径：纯 ASCII 且无大写字节的 token 原样 borrowed 返回（零分配）；其余走 `String::from_utf8_lossy` + Unicode `to_lowercase`。`normalizes()` 恒 `true`。

### `crates/core/src/analysis/analyzer.rs`（新建）
- `pub type TokenizerFactory = Arc<dyn for<'a> Fn(&'a str) -> Box<dyn Tokenizer<'a> + 'a> + Send + Sync>`
- `pub enum TokenizerTemplate { Whitespace, Letter, Keyword, Custom(TokenizerFactory) }`（`Clone`）
- `pub enum FilterKind { Lowercase, Custom(Arc<dyn TokenFilter>) }`（`Clone`），内部 `apply`/`normalizes` 静态分发到 `LowercaseFilter` 或自定义 filter
- `enum ActiveTokenizer<'a>`（私有）— 四种 tokenizer 的静态分发 + Custom 逃生舱
- `pub struct Analyzer { pub(crate) tokenizer, pub(crate) filters }`（`Clone`）
  - `analyze<'a, 'b>(&'b self, input: &'a str) -> TokenStream<'a, 'b>`
  - `normalize<'a>(&self, input: &'a str) -> Cow<'a, str>` — 不分词，整个输入作为单 token 过 normalizing filters；结果被 filter 丢弃时返回空串
- `pub struct TokenStream<'a, 'b>`，`impl Iterator<Item = Cow<'a, [u8]>>` — filter 返回 `None` 时跳过该 token（`'outer` 标签 continue）

### `crates/core/src/analysis/mod.rs`（修改）
最终形态与 brief 一致：`mod analyzer; mod filter; mod tokenizer;` + 三组 re-export
（`Analyzer, FilterKind, TokenStream, TokenizerFactory, TokenizerTemplate` / `LowercaseFilter, TokenFilter` / tokenizer 三项）。

## TDD 过程

1. 先写 analyzer.rs 测试模块（5 个测试）+ mod.rs 只声明 `mod analyzer;`：`cargo test -p rustlucene-core analysis` → 编译失败（E0422/E0425/E0433，15 errors），符合预期。
2. 写入 filter.rs 与 analyzer.rs 完整实现、更新 mod.rs → 测试通过。

## 测试命令与输出摘要

- `RUST_MIN_STACK=4194304 cargo test -p rustlucene-core analysis`
  → `test result: ok. 8 passed; 0 failed`（Task 1 的 3 项 + 本任务 5 项）
- `RUST_MIN_STACK=4194304 cargo test -p rustlucene-core`（全量）
  → lib `170 passed; 0 failed; 1 ignored`；两个 bin target `1 passed` / `2 passed`；doc-tests 0。既有测试零改动全绿。
- 警告检查：`cargo check` / 测试构建仅有 3 条预存警告（`pattern_chars`/`matches`/`glob_match` 未使用，位于与本次改动无关的文件），本次新增代码零警告。

## 自审（对照 brief 接口逐项核对）

- `TokenFilter` trait 签名（含 `Send + Sync`、两个方法签名）✓ 逐字一致
- `LowercaseFilter` 单元结构体 ✓
- `FilterKind` / `TokenizerTemplate` 枚举变体与 `Clone` derive ✓
- `TokenizerFactory` 类型别名（HRTB `for<'a>` + `Send + Sync`）✓
- `Analyzer` 字段可见性 `pub(crate)`、`Clone` ✓
- `analyze` / `normalize` 签名与生命周期 ✓
- `TokenStream<'a, 'b>` + `Iterator<Item = Cow<'a, [u8]>>` ✓
- mod.rs 与 brief 给的最终形态逐字一致 ✓
- `#![forbid(unsafe_code)]` 合规：全部 safe Rust，无新依赖 ✓

## 疑虑

无。代码与 brief 逐字一致，未做任何调整。

## 提交说明

- 本文件覆盖了上一个计划（codec 批读）遗留的同名 `task-2-report.md`——analyzer 框架计划复用了 task-N 编号。
- `.superpowers/sdd/.gitignore` 内容为 `*`，但 `task-2-brief.md` / `task-2-report.md` 均已被 git 追踪（此前 force-add），故 `git add -f` 正常生效，无需删除 .gitignore。
