# Task 5 报告：DocWriter 索引侧接入

日期：2026-08-03 · 分支：dev · 执行：subagent（TDD）

## 目标回顾

DocWriter 写入热路径接入 analyzer：带 `FieldSpec.analyzer` 的 text 字段在索引时经
`crate::analysis::Analyzer` 链归一化（`ERROR`/`error` → 同一 term）；无 analyzer 的
字段保持原有 `WhitespaceTokens` 快路径，字节级行为不变。

## TDD 过程

### Step 1–2：失败测试（先红）

按 brief 逐字追加两个测试到 `crates/core/src/doc_writer.rs` 的 `mod tests`：

- `analyzer_normalizes_terms_at_index_time`
- `no_analyzer_keeps_original_bytes`

运行 `cargo test -p rustlucene-core doc_writer`：

```
failures:
    doc_writer::tests::analyzer_normalizes_terms_at_index_time
test result: FAILED. 9 passed; 1 failed
```

失败形态符合预期：`assert_eq!(dict.len(), 2)` 得到 `left: 3, right: 2`
（词典仍是 `ERROR`/`Failed`/`error` 三个原始 term，未归一化）。
`no_analyzer_keeps_original_bytes` 此时即通过，作为不变性基线保留。

### Step 3：实现

全部改动集中在 `crates/core/src/doc_writer.rs`，按 brief 代码逐字落地：

1. **`FieldBuf` 新增字段**（原 line 283 后）：
   `pub analyzer: Option<crate::analysis::Analyzer>`，附 brief 给的 doc comment。
2. **`FieldBuf::new` 构造**：`spec.analyzer.as_deref().map(|a| Analyzer::parse(a)
   .expect("analyzer spec validated by Schema::add"))`——每个 field 每个 writer
   编译一次，跨 doc 复用；`Schema::add` 已校验 spec，此处 panic 不可达。
3. **自由函数 `index_token`**（放在 RAM 常量之后、`impl DocWriter` 之外）：
   提取原循环体的「词典插入 + posting 追加 + RAM delta」三段逻辑，返回 RAM delta。
4. **写入热路径替换**：原 `for token in WhitespaceTokens::new(&text)` 循环替换为
   `match &buf.analyzer` 双分支；两支共享 `index_token`、`saw_term`、`position`
   语义与 `doc_count` 累加。

借用方面：brief 提示的 `self.ram_bytes += ...` 与 `buf`/`dict` 借用共存无需调整——
`self.buffers` 与 `self.ram_bytes` 字段不相交，`buf.dict`（&mut）与 `buf.analyzer`
（&）字段不相交，NLL 字段级借用一次通过编译。

### Step 4：全绿验证

```
RUST_MIN_STACK=4194304 cargo test -p rustlucene-core
test result: ok. 179 passed; 0 failed; 1 ignored   (lib)
test result: ok. 1 passed; 0 failed                (其他 target)
test result: ok. 2 passed; 0 failed                (rustlucene-cli)
```

lib 测试 170 → 179：新增 2 项 + 既有 177 项全部通过，**既有测试零改动**。
`cargo build -p rustlucene-core` 无新警告（仅 3 条既有 dead-code 警告，
`pattern_chars`/`matches`/`glob_match`，与本改动无关）。

## 自审：无 analyzer 分支逐语句等价核对（字节级不变约束）

逐项对照原循环（原 line 399-414）与新 None 分支 + `index_token`：

| 项 | 原循环 | 新代码 | 一致 |
|---|---|---|---|
| 词典插入 | `dict.lookup_or_insert_flag(tok)` | 同一调用（helper 内） | ✓ |
| posting 追加 | `add_occurrence(doc_id, if has_positions { Some(position) } else { None })` | 同一调用、同一参数表达式 | ✓ |
| 新 term RAM | `if is_new { TERM_RAM + tok.len() } else { 0 }` | helper 内逐字一致 | ✓ |
| posting RAM | `if new_doc { POSTING_NEW_DOC_RAM } else { POSTING_SAME_DOC_RAM }` | helper 内逐字一致 | ✓ |
| RAM 累加 | `self.ram_bytes += <两项之和>` | `self.ram_bytes += index_token(...)`（返回同一和） | ✓ |
| position 语义 | 0 起、每 token `+= 1` | 不变 | ✓ |
| saw_term/doc_count | 有 token 则 `doc_count += 1` | 不变 | ✓ |
| token 来源 | `WhitespaceTokens::new(&text)` | None 分支不变 | ✓ |

`is_new`/`new_doc`/`tok.len()` 均取自同一调用序列，无重排、无额外分配；
无 analyzer 字段的词典内容、posting、RAM 记账与改动前完全相同。

## analyzer 路径行为

- `analyzer.analyze(&text)` 产出 `TokenStream`（`Iterator<Item = Cow<[u8]>>`），
  `&tok` 解引用为 `&[u8]` 进 `index_token`。
- position 按 analyzer 输出 token 序 0,1,2… 递增，与 whitespace 路径同一语义。
- analyzer 输出为空（如全被过滤）时 `saw_term == false`，`doc_count` 不增，
  与空文本的 whitespace 行为一致。

## 提交

```
git add crates/core/src/doc_writer.rs
git add -f .superpowers/sdd/task-5-brief.md .superpowers/sdd/task-5-report.md
git commit -m "core: run configured analyzer chain at index time in DocWriter"
```

## 疑虑 / 后续

- 未运行 `make log-test`（耗时较长，任务要求以 cargo test 为准）；字节级不变的
  保障来自上述逐语句等价，如需端到端确认可补跑。
- `Analyzer::parse` 的 `expect` 依赖 `Schema::add` 已校验这一前置（Task 4 已落地，
  schema.rs:215-222 有校验与测试）。若未来存在绕过 `Schema::add` 构造 FieldBuf
  的路径，`expect` 会 panic 而非静默降级——当前无此路径。
- Task 6（查询侧接入）尚未开始。
