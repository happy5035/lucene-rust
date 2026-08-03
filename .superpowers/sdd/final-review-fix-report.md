# Analyzer 框架最终 code review 修复报告

分支：dev。审查结论 With fixes，本报告记录全部修复项、测试命令与结果。

## 修复 1（Important）：LowercaseFilter ASCII 特判

- 文件：`crates/core/src/analysis/filter.rs`
- 问题：fast path（纯 ASCII 无大写 → borrowed）之后，其余一律走
  `String::from_utf8_lossy(...).into_owned().to_lowercase()`，ASCII 含大写
  的常见路径（ERROR、NullPointerException 等日志级别/类名）产生双分配且
  经过 Unicode 表。
- 改动：在 fast path 之后、Unicode 回退之前插入 ASCII 分支：
  `token.is_ascii()` → `Cow::Owned(token.to_ascii_lowercase())`，单分配、
  不查 Unicode 表。同步更新结构体 doc 注释描述三级路径。
- 测试：`crates/core/src/analysis/analyzer.rs` 新增
  `lowercase_ascii_uppercase_single_alloc`：`"ERROR"` 经 whitespace+lowercase
  链分析 → 内容为 `error` 且为 `Cow::Owned`。

## 修复 2（Important）：registry 拒绝内置名注册

- 文件：`crates/core/src/analysis/registry.rs`
- 问题：`register_tokenizer` / `register_filter` 允许用内置名注册，注册的
  自定义组件永远不会被命中（parse 先匹配内置名），属于静默无效配置。
- 改动：`register_tokenizer` 开头
  `assert!(!matches!(name, "whitespace" | "letter" | "keyword"), "cannot override built-in tokenizer: {name}")`；
  `register_filter` 开头
  `assert!(!matches!(name, "lowercase"), "cannot override built-in token filter: {name}")`。
- 测试：registry.rs 测试模块新增
  `builtin_tokenizer_names_cannot_be_overridden`（三个内置名逐一
  catch_unwind 验证 panic）与 `builtin_filter_names_cannot_be_overridden`，
  写法对齐 `schema.rs` 的 `analyzer_only_on_indexed_tokenized_fields`。

## 修复 3（文档同步）：spec 查询侧锁问题段落

- 文件：`docs/superpowers/specs/2026-07-31-analyzer-framework-design.md`
- 改动：「查询侧双通道」的「锁问题」段落由「writer 构建时只预解析规格文
  本（→ Vec<ComponentSpec>），每查询 new 一组无状态组件」更新为实际实现：
  `analyze_query` 的 `analyzer_for` 每查询对规格文本调 `Analyzer::parse`
  全量重解析（无状态组件，含一次注册表读锁 + HashMap 查找，成本同阶，
  <100 QPS 目标下无感）。

## 修复 4（文档）：README 组件名字符集说明

- 文件：`README.md`「分析器」表格行
- 改动：补一句：自定义组件名不得含 `+` `,` `:` `|`（与 schema spec 分隔符
  冲突）。

## 测试

命令：`RUST_MIN_STACK=4194304 cargo test -p rustlucene-core analysis`

结果：`test result: ok. 22 passed; 0 failed; 0 ignored`（lib target；
两个 bin target 无匹配用例）。新增 3 条用例全部通过：

- `analysis::analyzer::tests::lowercase_ascii_uppercase_single_alloc ... ok`
- `analysis::registry::tests::builtin_tokenizer_names_cannot_be_overridden ... ok`
- `analysis::registry::tests::builtin_filter_names_cannot_be_overridden ... ok`
