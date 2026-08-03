# Task 1 报告：analysis 模块骨架 + 三个内置 tokenizer

## 做了什么

按 brief 逐字实现：

- 新建 `crates/core/src/analysis/tokenizer.rs`：`Tokenizer<'a>` trait（`next_token(&mut self) -> Option<&'a [u8]>`，零拷贝借用输入）、`WhitespaceTokens`（包装 `SplitAsciiWhitespace`，同时保留 `Iterator<Item = &'a str>` 供 doc_writer 现状路径）、`LetterTokens`（按 `char::is_alphabetic` 最大连续段切分）、`KeywordTokens`（整体一个 token，空输入无 token）。实现与测试代码均逐字取自 brief。
- 新建 `crates/core/src/analysis/mod.rs`：骨架模块，re-export 四个公开项。
- `crates/core/src/lib.rs:21`：`pub mod tokenizer;` → `pub mod analysis;`。
- `crates/core/src/doc_writer.rs:7`：`use crate::tokenizer::WhitespaceTokens;` → `use crate::analysis::WhitespaceTokens;`。
- 删除 `crates/core/src/tokenizer.rs`（`git rm`）。doc_writer 的 `for token in WhitespaceTokens::new(&text)` 调用点（doc_writer.rs:399）经 Iterator impl 无缝工作，未改动。
- 无新依赖；全部 safe Rust（`#![forbid(unsafe_code)]` 不受影响）。

## TDD 过程

1. 先写测试模块（含 `analysis/mod.rs` 声明与 lib.rs 切换，否则测试文件不参与编译、无法失败），运行
   `RUST_MIN_STACK=4194304 cargo test -p rustlucene-core analysis::tokenizer` → 按预期编译失败（E0432/E0433/E0405：`Tokenizer`、`WhitespaceTokens`、`LetterTokens`、`KeywordTokens` 未定义）。
2. 写入 brief 的实现后 `git rm crates/core/src/tokenizer.rs`，全量测试通过。

## 测试命令与输出摘要

命令：`RUST_MIN_STACK=4194304 cargo test -p rustlucene-core`

```
test analysis::tokenizer::tests::keyword_yields_whole_input_once ... ok
test analysis::tokenizer::tests::letter_splits_on_non_alphabetic ... ok
test analysis::tokenizer::tests::whitespace_splits_on_ascii_whitespace ... ok
test result: ok. 165 passed; 0 failed; 1 ignored   (lib)
test result: ok. 1 passed; 0 failed                (另一 target)
test result: ok. 2 passed; 0 failed                (rustlucene-cli)
test result: ok. 0 passed; 0 failed                (doc-tests)
```

全部绿色，0 失败。既有测试零改动：`git diff HEAD~1 --stat` 对 crates/ 仅显示 lib.rs、doc_writer.rs 各 1 行 import 变更、tokenizer.rs 删除（其旧测试 `splits_on_whitespace` 的用例被新测试 `whitespace_splits_on_ascii_whitespace` 覆盖，属 brief 要求的迁移）。

## 自审（对照 brief 逐项）

- [x] Create `analysis/tokenizer.rs`（实现+测试逐字）
- [x] Create `analysis/mod.rs`（逐字）
- [x] Delete `crates/core/src/tokenizer.rs`
- [x] `lib.rs:21` → `pub mod analysis;`
- [x] `doc_writer.rs:7` import 路径
- [x] Produces 接口四项全部就位（trait + 三个 struct + WhitespaceTokens 的 Iterator 保留）
- [x] 无新依赖（Cargo.toml/Cargo.lock 未动）
- [x] 全量测试绿、既有测试未改动

## 疑虑 / 说明

- brief 代码与 rustfmt 有细微出入（如 `Self { input: text, offset: 0 }` 单行、import 字母序），但基线本身已有 88 处既有 fmt drift（含 doc_writer.rs），项目未强制 fmt；按要求逐字使用 brief 代码，未另行格式化。
- 操作插曲：验证 fmt 基线时用了 `git stash`/`git stash pop`，导致已暂存的 tokenizer.rs 删除被退回未暂存状态，已用 `git add -A` 重新暂存后提交，最终 commit 内容经 `git show --stat` 确认完整。
- `.superpowers/sdd/.gitignore`（内容为 `*`）存在，但 task-1-brief.md / task-1-report.md 此前已被跟踪，`git add -f` 正常工作，未删除该 .gitignore。
