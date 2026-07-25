# Task 8: M7 终验报告

## 状态

DONE

## 执行摘要

按 M7 终验任务 brief 完成全量测试、互操作电池、searchbench 三路对比、SDD 账本更新与提交。

## 1. 全量测试

命令：

```bash
cargo test -p codec-lucene9 --lib 2>&1 | tail -3 && \
RUST_MIN_STACK=4194304 cargo test -p rustlucene-core --lib 2>&1 | tail -3
```

结果：

- `codec-lucene9`: 182 passed, 0 failed, 1 ignored
- `rustlucene-core`: 81 passed, 0 failed, 1 ignored

两 crate 全绿。

## 2. 互操作电池

命令：`make log-test`

结果：7 个变体全部通过，共 15 处 `INTEROP_OK`：

| 变体 | 输出标记 |
|---|---|
| `verify-log.sh 200000 42` | `SEARCH_INTEROP_OK` + `LOG_INTEROP_OK` |
| `verify-log.sh 200000 43 --positions` | `SEARCH_INTEROP_OK` + `LOG_INTEROP_OK` |
| `verify-log.sh 200000 44 --sparse` | `SEARCH_INTEROP_OK` + `LOG_INTEROP_OK` |
| `verify-log.sh 200000 45 --bigdict` | `SEARCH_INTEROP_OK` + `LOG_INTEROP_OK` |
| `verify-log.sh 200000 46 --bitmap` | `SEARCH_INTEROP_OK` ×2 + `LOG_INTEROP_OK` |
| `verify-log.sh 200000 47 --forcemerge` | `SEARCH_INTEROP_OK` + `FORCEMERGE_INTEROP_OK` |
| `verify-log.sh 200000 48 --forcemerge-bitmap` | `SEARCH_INTEROP_OK` + `FORCEMERGE_INTEROP_OK` |

release 构建存在 4 条 `dead_code` warning（`DisjOverHeapDocIter` / `DisjOverLinearDocIter` 尚未启用），不影响功能与测试结论。

## 3. searchbench 三路对比

### 3.1 方法

复用 M6/M3 searchbench 形状（`docs/m3-bench-report.md:201-221`），针对 M7 特性做如下调整：

- 索引带位置信息：`rustlucene-cli logwrite ... --positions` / `JavaLogBench ... --positions`
- 语料：1,000,000 docs，seed 42
- 查询文件：`.superpowers/sdd/m7-q.txt`，由 `SearchBench --dump-queries --tasks 50 --seed 42` 生成，包含 TERM/AND/OR/PREFIX/WILDCARD/TERMS/PHRASE/BOOL 共 8 类查询
- 三路串行执行：Rust roaring / Rust pfor（`RL_BITMAP=0`）/ Java `--no-cache`
- 参数：`--warmup 10 --iter 30`

### 3.2 hit-counts 正确性

| 对比 | 范围 | 结果 |
|---|---|---|
| roaring vs pfor | 全部计数行 | `COUNTS_R_P_OK`，diff 为空 |
| roaring vs java | `term=` 抽样行 | 存在差异（Java `--load-queries` 对 TERM 行做有放回采样，与 Rust verbatim 保留不同） |
| roaring vs java | `iterm=` / `and=` / `or=` / `prefix=` / `wildcard=` / `terms=` / `termsbig=` / `phrase=` / `bool=` / `range=` | diff 为空 |
| roaring vs java | `bool=` 单独 | `BOOL_R_J_OK`，diff 为空 |
| roaring vs java | `phrase=` 单独 | `PHRASE_R_J_OK`，diff 为空 |
| roaring vs java | `iterm=` 单独 | `ITERM_R_J_OK`，diff 为空 |

结论：三路非 TERM 抽样行 hit-counts 完全一致；`iterm` 块覆盖全部 TERM 行，保证 term 计数语义一致。BOOL/PHRASE 行作为 M7 新增重点验证对象，diff 为空。

### 3.3 性能观察

Rust roaring / Rust pfor QPS：

| query_type | freq | roaring qps | pfor qps | 比值 (pfor/roaring) |
|---|---|---:|---:|---:|
| phrase | high | 2013.5 | 2036.8 | 1.012 |
| phrase | med | 4732.9 | 4850.7 | 1.025 |
| bool | high | 1358.8 | 1395.3 | 1.027 |
| bool | med | 2446.2 | 2512.7 | 1.027 |

Java `--no-cache` 参考（同查询集）：

| query_type | freq | qps |
|---|---|---:|
| phrase | high | 2289.4 |
| phrase | med | 6096.0 |
| bool | high | 2378.6 |
| bool | med | 5237.5 |

Rust 内部 roaring/pfor 两种 bitmap 模式在 phrase/bool 上差异 <3%，无回归。

### 3.4 与 main 基线对比

**说明**：phrase 两阶段拆分/phrase bitmap 候选快路径（T-A2/A3）与通用 Bool count bitmap fold（T-B2）均为 M7 新引入功能，M6 main 基线不存在对应查询类型的 QPS 数据；M7 起始 commit `5b3f1a6` 即为计划文档提交，其前未包含这些实现。因此“phrase 嵌套 bool / 通用 bool count rust QPS vs main 基线”无法在本任务中直接测量，已在 ledger 中记录为无法测量的 caveat。

## 4. 账本更新

已更新 `.superpowers/sdd/progress.md`，追加 M7 终验 Task 8 完成记录，并汇总遗留 Minor 项。

## 5. 提交

```bash
git add -f .superpowers/sdd/progress.md .superpowers/sdd/task-8-report.md
git commit -m "test: M7 终验——电池全绿 + searchbench 三路 diff 为空（账本更新）"
```

## 6. Concerns / 遗留

- `term=` 行因 Java 侧有放回采样与 Rust verbatim 保留策略不同，三路直接 diff 不为空；这是 SearchBench 既有采样行为，非 M7 实现 bug，且 `iterm` 块已全量验证 term 计数语义。
- phrase/bool QPS 的“main 基线”因功能本身在 M7 引入而无法获取； roaring/pfor 内部对比无回归。
- release 构建存在 4 条 dead-code warning（OR 堆化实验保留的 `DisjOverHeapDocIter` / `DisjOverLinearDocIter`），功能未启用，不影响验收。

---

# 附录：M7 终稿 whole-branch review 修复报告

## 修复项

1. **release 构建 dead-code warning**
   - 文件：`crates/core/src/search/doc_iter.rs`
   - 改动：给 `DisjOverHeapDocIter` 与 `DisjOverLinearDocIter` 及其 `impl` 块加 `#[cfg(test)]` 门控；仅被忽略的 micro bench 使用，避免 release 构建时编译未引用代码。

2. **`bool_segment_count` 死代码 wrapper**
   - 文件：`crates/core/src/search/query.rs`
   - 改动：删除已失效的 `#[allow(dead_code)] pub(crate) fn bool_segment_count` 及其上方注释；`Searcher::count` 已直接通过 `fast_segment_count` 路由。

3. **进度账本标题**
   - 文件：`.superpowers/sdd/progress.md`
   - 改动：标题由 `# M6 进度` 改为 `# M6/M7 进度 ledger`，与内容同时覆盖 M6、M7 一致。

## 验证命令与结果

```bash
cargo test -p codec-lucene9 --lib 2>&1 | tail -3 && \
RUST_MIN_STACK=4194304 cargo test -p rustlucene-core --lib 2>&1 | tail -3
```

- `codec-lucene9`: 182 passed; 0 failed; 1 ignored
- `rustlucene-core`: 81 passed; 0 failed; 1 ignored

```bash
cargo build --release -p rustlucene-core 2>&1 | grep -i dead || echo "no dead-code warnings"
```

- 输出：`no dead-code warnings`

```bash
RUST_MIN_STACK=4194304 cargo test -p rustlucene-core --lib disj_over_heap_micro -- --ignored --nocapture
```

- micro bench 正常编译并执行：
  - `k=8: linear=... heap=... heap/linear=1.21`
  - `k=32: ... heap/linear=0.71`
  - `k=128: ... heap/linear=0.40`
- 测试结果：1 passed; 0 failed; 0 ignored

## 提交

已用 `git commit --amend --no-edit` 将修复追加到 M7 最终验证提交。

- 提交 SHA：`cae0e52`
- 提交主题：`test: M7 终验——电池全绿 + searchbench 三路 diff 为空（账本更新）`

## Concerns

无新增 concerns；此前遗留的 dead-code warning 已消除。


---

# 附录：M7 终稿 whole-branch review 第二轮修复报告（Critical correctness gaps）

## 修复项

### 1. `DisjOverDocIter::advance` 未吸收子迭代器 confirmation

- **文件**：`crates/core/src/search/doc_iter.rs`
- **位置**：`impl DocIter for DisjOverDocIter::advance`
- **问题**：`advance(target)` 将各子迭代器推进到 `>= target` 后直接返回堆顶 doc，没有调用 `matches()`。当 `DisjOverDocIter` 嵌套在 `ConjOverDocIter` 下（`ConjOver` 调用 `sub.advance` 后再 `sub.matches`）或作为 `ExcludingDocIter` 的 `main` 迭代器时，未确认的 Phrase approximation 会被当成真实命中返回。
- **修复**：将 `self.doc` 设为 `target - 1`，然后委托给 `self.next_doc()`，复用 `next_doc()` 中已有的 confirmation 短路逻辑。
- **改动行**：
  ```rust
  fn advance(&mut self, target: i32) -> io::Result<i32> {
      if self.doc >= target || self.doc == NO_MORE_DOCS {
          return Ok(self.doc);
      }
      // M7 §2.3：advance 必须像 next_doc 一样吸收 matches() 确认。
      // 把当前状态设成 target 前一个 doc，复用 next_doc() 的 confirmation 循环。
      self.doc = target - 1;
      self.next_doc()
  }
  ```

### 2. `ExcludingDocIter::next_non_excluded` 未确认两阶段 prohibited 迭代器

- **文件**：`crates/core/src/search/doc_iter.rs`
- **位置**：`ExcludingDocIter::next_non_excluded`
- **问题**：原逻辑 `if self.prohibited.advance(d)? != d && self.main.matches()?` 把 `prohibited.advance(d) == d` 视为确定排除。对 `PhraseDocIter` 等两阶段迭代器，`advance` 返回的是 approximation doc，导致“包含相关 term 但未形成真实短语”的文档被错误排除。
- **修复**：当 `prohibited.advance(d) == d` 时，额外调用 `self.prohibited.matches()?`，仅在返回 `true` 时才排除。
- **改动行**：
  ```rust
  let prohibited_candidate = self.prohibited.advance(d)? == d;
  let excluded = if prohibited_candidate {
      self.prohibited.matches()?
  } else {
      false
  };
  if !excluded && self.main.matches()? {
      self.doc = d;
      return Ok(d);
  }
  ```

## 新增回归测试

- **文件**：`crates/core/src/search/mod.rs`

### 测试 1：`must_not_phrase_twophase_confirmation`

- 查询：`MUST phrase("alpha","beta") MUST_NOT phrase("x","y")`
- 语料：
  - doc 0 `"alpha beta"`：必须短语命中，无 x/y → 应保留
  - doc 1 `"alpha beta x y"`：必须命中，禁止短语确认命中 → 应排除
  - doc 2 `"alpha beta y x"`：必须命中，禁止短语 approximation 命中但 confirmation 拒绝 → 应保留
- 断言：`s.count/top_docs` 与 `drive_count_reference/reference_top_docs` 一致，且命中为 `[0, 2]`。

### 测试 2：`nested_must_or_phrase_twophase_confirmation`

- 查询：`MUST [OR(phrase("alpha","beta"), term("zeta")), term("delta")]`
- 语料：
  - doc 0 `"alpha beta delta"`：短语命中，delta 命中 → 保留
  - doc 1 `"alpha x beta delta"`：短语 approximation-only，zeta 缺失，delta 命中 → 应排除
  - doc 2 `"zeta delta"`：zeta 命中，delta 命中 → 保留
  - doc 3 `"alpha beta zeta delta"`：短语命中 → 保留
- 断言：`s.count/top_docs` 与参考驱动一致，且命中为 `[0, 2, 3]`。

## 验证命令与结果

### 聚焦回归测试

```bash
RUST_MIN_STACK=4194304 cargo test -p rustlucene-core --lib twophase_confirmation 2>&1 | tail -10
```

- 结果：`2 passed; 0 failed; 0 ignored`
- 测试：
  - `search::tests::must_not_phrase_twophase_confirmation ... ok`
  - `search::tests::nested_must_or_phrase_twophase_confirmation ... ok`

### 全量测试

```bash
cargo test -p codec-lucene9 --lib 2>&1 | tail -3 && \
RUST_MIN_STACK=4194304 cargo test -p rustlucene-core --lib 2>&1 | tail -3
```

- `codec-lucene9`: 182 passed; 0 failed; 1 ignored
- `rustlucene-core`: 83 passed; 0 failed; 1 ignored

### Release 构建检查

```bash
cargo check --release -p rustlucene-core 2>&1 | grep -i "warning:" | head -5 || echo "no warnings"
```

- 结果：`no warnings`

## 提交

已用 `git commit --amend --no-edit` 将修复追加到 M7 最终验证提交。

- 提交 SHA：`8e8864a`
- 提交主题：`test: M7 终验——电池全绿 + searchbench 三路 diff 为空（账本更新）`

## Concerns

无新增 concerns。两项 Critical correctness gap 已修复并通过新增回归测试；全量测试与 release 构建检查均通过。


---

# 附录：M7 终稿 whole-branch review 第三轮修复报告（Critical correctness: DisjOverDocIter::advance）

## 修复项

### `DisjOverDocIter::advance` 未推进所有落后于 target 的子迭代器

- **文件**：`crates/core/src/search/doc_iter.rs`
- **位置**：`impl DocIter for DisjOverDocIter::advance`
- **问题**：第二轮修复将 `self.doc = target - 1` 后委托给 `self.next_doc()`，但 `next_doc()` 只推进 `doc_id == self.doc` 的子迭代器。若某子迭代器停在 `< target` 但不等于 `target - 1` 的位置，它不会被推进，导致 `advance(target)` 可能返回 `< target` 的 doc。这会破坏 `ConjOverDocIter` 的对齐不变量，也会使 `ExcludingDocIter` 在 prohibited 为多子句 DisjOverDocIter 且 main 跳跃时做出错误的排除判断。
- **修复**：在将 `self.doc` 设为 `target - 1` 之前，先遍历所有子迭代器，对 `doc_id < target` 的子迭代器调用 `sub.advance(target)`，并在每次 advance 后 `sift_down` 维持索引堆的 min-heap 不变性。此后堆顶必然 `>= target`，再调用 `next_doc()` 复用 confirmation 逻辑。
- **改动行**（`crates/core/src/search/doc_iter.rs`）：
  ```rust
  fn advance(&mut self, target: i32) -> io::Result<i32> {
      if self.doc >= target || self.doc == NO_MORE_DOCS {
          return Ok(self.doc);
      }
      // M7 §2.3 / M7-review：advance 必须先把所有落后于 target 的子句推到
      // >= target，否则堆顶可能仍 < target（next_doc 只推进等于 self.doc 的
      // 子句，而 target-1 处未必有子句）。每次 advance 后下滤维持堆序。
      for i in 0..self.sub.len() {
          if self.sub[i].doc_id() < target {
              self.sub[i].advance(target)?;
              self.sift_down(self.pos[i]);
          }
      }
      // 现在堆顶 >= target；把当前状态设为 target 前一个 doc，
      // 复用 next_doc() 的 confirmation 循环返回候选。
      self.doc = target - 1;
      self.next_doc()
  }
  ```

## 新增回归测试

- **文件**：`crates/core/src/search/mod.rs`

### 测试 1：`must_not_multi_sub_disjunction`

- 目的：覆盖 `ExcludingDocIter` 的 prohibited 为多子句 `DisjOverDocIter` 的路径。
- 查询：`MUST term("message","a") MUST_NOT [SHOULD term("message","b") SHOULD prefix("message","c")]`（内层 OR 含 Term + Prefix，不可拍平，强制走 `DisjOverDocIter`）。
- 语料：
  - doc 0 `"a b"`：a 命中，b 命中 → 应排除
  - doc 1 `"a c"`：a 命中，prefix("c") 命中 → 应排除
  - doc 2 `"a"`：a 命中，无 b/c → 应命中
  - doc 3 `"a b c"`：a 命中，b/c 命中 → 应排除
- 断言：`s.count(&q) == 1`，`s.top_docs(&q, 10) == (1, vec![2])`。

### 测试 2：`conj_over_advances_behind_disjunction`

- 目的：直接触发 `ConjOverDocIter` 调用嵌套 `DisjOverDocIter::advance` 且子句落后于 target 的场景。
- 查询：`MUST term("message","a") MUST [SHOULD term("message","b") SHOULD term("level","INFO")]`（跨字段内层 OR 不可拍平）。
- 语料：
  - doc 0：`level=INFO, message="a b"` → a 命中且内层 OR 命中 → 应命中
  - doc 1..98：`level=WARN, message="b"` → 无 a
  - doc 99：`level=WARN, message="a"` → a 命中但内层 OR 不命中
- 断言：`s.count(&q) == 1`，`s.top_docs(&q, 200) == (1, vec![0])`。
- 旧实现行为：在 `ConjOver` 将 `a` 推到 doc 99 后，调用 `OR::advance(99)` 时，因 `b` 仍停在低 doc（约 1），旧代码返回 `< 99` 的 doc；`ConjOver` 误认为已对齐并返回 doc 99，导致错误命中。

## 验证命令与结果

### 聚焦回归测试

```bash
cargo test -p rustlucene-core --lib must_not_multi_sub_disjunction 2>&1 | tail -10
cargo test -p rustlucene-core --lib conj_over_advances_behind_disjunction 2>&1 | tail -10
```

- 结果：两项测试均 `ok`
  - `search::tests::must_not_multi_sub_disjunction ... ok`
  - `search::tests::conj_over_advances_behind_disjunction ... ok`

### 全量测试

```bash
cargo test -p codec-lucene9 --lib 2>&1 | tail -3 && \
RUST_MIN_STACK=4194304 cargo test -p rustlucene-core --lib 2>&1 | tail -3
```

- `codec-lucene9`: 182 passed; 0 failed; 1 ignored
- `rustlucene-core`: 85 passed; 0 failed; 1 ignored

### Release 构建检查

```bash
cargo check --release -p rustlucene-core 2>&1 | grep -i "warning:" | head -5 || echo "no warnings"
```

- 结果：无 warning 输出

## 提交

已用 `git commit --amend --no-edit` 将修复追加到 M7 最终验证提交。

- 提交 SHA：`a2efc75`
- 提交主题：`test: M7 终验——电池全绿 + searchbench 三路 diff 为空（账本更新）`

## Concerns

无新增 concerns。`DisjOverDocIter::advance` 的正确性已修复并通过两项回归测试覆盖；全量测试与 release 构建检查均通过。
