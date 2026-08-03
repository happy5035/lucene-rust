# Task 8 报告：端到端测试 + README + 回归验证

## 变更内容

1. **E2E 测试**（`crates/core/src/analysis/query_analysis.rs` 测试模块追加 `lowercase_write_search_roundtrip`）：
   通过 spec 字符串建 schema（`message:text+positions+analyzer=whitespace|lowercase,level:keyword`），
   写入一份混合大小写文档，验证写入→索引侧分析→`analyze_query` 查询重写→搜索闭环：
   - analyzed 字段：小写查询 `error` 命中索引中归一化后的 term（total=1）
   - terms IN 走同一重写：`Query::terms("level", &["ERROR","WARN"])` total=1（level 无 analyzer，原样透传）
   - keyword 字段保持精确大小写语义：`error` total=0、`ERROR` total=1
2. **README.md** 三处最小修改：
   - 字段类型表追加「分析器」一行（`|` 按文件惯例转义为 `\|`）
   - JSON 绑定层一行补 `analyzer=` modifier 说明
   - 搜索读路径一节注明：查询侧分析只在 JNI `nativeSearch` 生效，CLI/磁盘 Searcher 直查需调用方自行归一化（经 `analyze_query`）
   - 「暂不支持」列表中无大小写归一化/分析器相关条目，无需改动
3. **`crates/core/src/json.rs`** 文件头 doc comment 补 `analyzer=` modifier 一句。

既有测试零改动；README/json.rs 注释之外不动任何既有代码。

## 回归结果

### 1. `RUST_MIN_STACK=4194304 cargo test --workspace`

全绿（exit 0），含新增 E2E 测试：

```
test result: ok. 218 passed; 0 failed; 1 ignored   (codec-lucene9)
test result: ok. 187 passed; 0 failed; 1 ignored   (rustlucene-core，含新增 1 个 E2E)
test result: ok. 1 passed  (core doc/其他 target)
test result: ok. 2 passed
test result: ok. 12 passed
test result: ok. 47 passed (rustlucene-metric)
test result: ok. 4 passed  (rustlucene-jni)
其余 bin target 0 测试，全部 ok
```

`cargo test -p rustlucene-core analysis::query_analysis`：8 passed / 0 failed。

### 2. `make log-test`

exit=0，全量输出统计：15 次 `No problems were detected with this index.`，
15 次 `INTEROP_OK`（SEARCH_INTEROP_OK / LOG_INTEROP_OK / FORCEMERGE_INTEROP_OK），无 FAIL / Error：

```
No problems were detected with this index.   ×15
SEARCH_INTEROP_OK / LOG_INTEROP_OK           ×6 组（五变体 + forcemerge）
FORCEMERGE_INTEROP_OK                        ×2
```

（brief 预期"五变体 11 次 No problems"，实测 15 次——脚本变体数量较 brief 编写时增多，全部通过。）
默认路径字节级不变的硬验证通过。完整日志：`/tmp/log-test-full.log`（本次会话内）。

### 3. logwrite 吞吐基线

```
$ cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- logwrite /tmp/idx-analyzer-bench 1000000 42
WROTE docs=1000000 elapsed_ms=5437 docs_per_sec=183925
```

对照 README 基线（单线程 182k docs/s 档）：183925 docs/s（+1.0%），±5% 内持平。
无 analyzer 路径零开销，无需给 helper 加 `#[inline]` 排查。
