# Task 3 Report: 名字注册表 + `Analyzer::parse`

## Status: DONE

## Commits
- `core: analyzer name registry and Analyzer::parse`（代码：`crates/core/src/analysis/`）
- `sdd: task-3 report (analyzer name registry)`（brief + 本报告）

## Test Summary
- 全绿：lib 173 passed / 0 failed / 1 ignored（含 analysis 11 个：registry 新增 3 个 + analyzer 5 + tokenizer 3），bin/cli 测试全过。
- 红灯验证：实现前 `cargo test -p rustlucene-core analysis` 编译失败（E0433/E0425：`Analyzer::parse`、`register_filter` 不存在）。
- 命令：`RUST_MIN_STACK=4194304 cargo test -p rustlucene-core`
- 3 个 warning 均为 `crates/core/src/search/multi_term.rs` 既存 dead-code 告警，与本任务无关。

## Files Changed
- `crates/core/src/analysis/registry.rs` — 新建。`custom_tokenizers()` / `custom_filters()` 两个 `OnceLock<RwLock<HashMap>>` 注册表；`register_tokenizer` / `register_filter`；`impl Analyzer { pub fn parse }`；brief 给定测试模块（3 个测试）。
- `crates/core/src/analysis/mod.rs` — 增加 `mod registry;` 和 `pub use registry::{register_filter, register_tokenizer};`。

## Self-Review（对照 brief 接口清单）
- [x] `Analyzer::parse(spec: &str) -> Result<Analyzer, String>` — inherent impl，签名逐字一致；`"tokenizer|filter|..."` 语法，链序即声明序。
- [x] `pub fn register_tokenizer(name: &str, factory: TokenizerFactory)` — 签名逐字一致。
- [x] `pub fn register_filter(name: &str, filter: Arc<dyn TokenFilter>)` — 签名逐字一致。
- [x] 实现代码逐字采用 brief Step 3（无改写）。
- [x] 测试代码逐字采用 brief Step 1（3 个测试全过）。
- [x] filter 位只查内置 filter 名（`lowercase`）+ 自定义 filter 注册表，不合并 tokenizer 名——`Analyzer::parse("whitespace|letter")` 返回 Err（`unknown token filter: letter`），测试覆盖。
- [x] 空 spec / 未知 tokenizer / 未知 filter 均报错，测试覆盖。
- [x] 全 safe Rust（`#![forbid(unsafe_code)]` 下编译通过），无新依赖（仅 std `OnceLock`/`RwLock`/`HashMap`）。
- [x] 既有测试零改动全绿（173 passed，baseline 170 + 新增 3）。
- [x] mod.rs re-export 与 brief 一致；`Analyzer::parse` 为 inherent impl 随 `Analyzer` 自动导出。

## 疑虑
无。
