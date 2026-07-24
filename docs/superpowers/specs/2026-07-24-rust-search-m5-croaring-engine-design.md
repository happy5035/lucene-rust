# M5 设计：croaring 引擎替换（Frozen view 零拷贝 + C 优化算子）

日期：2026-07-24。状态：已获用户批准（用户提出"直接考虑使用 croaring"，探针实测数据
验证后拍板；依赖政策变更——引入 croaring crate——经用户认账）。
前置：M3（inline bitmap）、M4（读侧去税；引擎回退已实测）、croaring 探针报告
（`/tmp/croaring-probe/REPORT.md`）。

## 1. 动机（全部实测数据）

- **M4 遗留回退**（`m4-bench-report.md`）：稀疏 AND 0.33x（字节游标对舞 ≈4.5x M3 折叠）、
  稠密 AND/OR 0.92x/1.34x（M3 容器折叠曾 8.7x/15.2x）。自研引擎修到 Java/croaring 水平的
  成本高。
- **croaring 探针**（2.7.0 + croaring-sys 4.7.1，ustc 镜像，release 11s 零错，**musl 可编可跑**）：
  - `and_cardinality` 两个 Frozen view 间：稀疏 **7.7µs**（自研 221.7 / Java 188.1）、稠密
    **4.2µs**（215.7 / 275.3）、run **0.27µs**（0.89 / 0.55）——快 25–65x。
  - **Frozen view 真零拷贝**：`BitmapView::deserialize::<Frozen>` 创建 ~60ns，借用 buffer
    后 and/or/contains/iter 全可用；vs 我们 M3 的 deserialize 244.9µs。
  - `contains` 8.5–28.7 ns/次（M4 probe 策略因此重新变得划算）；迭代 4.6–7.3 ns/doc。
- 结论（用户拍板）：保留 .doc 内联外壳与档判定，**引擎整体换成 croaring**，删除自研容器库。

## 2. 范围

- **写侧**：flush 钩点不动；bitmap 构建改 `Bitmap::of(sorted_docs)` → `shrink_to_fit()` →
  Frozen 序列化为 payload（`Bitmap::of` ~5.7ns/doc，写侧开销 bench 量化）。
- **读侧**：定位/三重校验（len 有界 → magic/version → df/card == termState）不动；区域读入
  **32B 对齐 buffer**（一次 memcpy，Frozen view 的对齐契约），`BitmapView::deserialize::<Frozen>`
  打开——unsafe 只此一处，codec 第三个模块级 allow。
- **引擎**：
  - Term：迭代 = view.iter；count = doc_freq（M4 已改，不变）。
  - AND/OR count = `and_cardinality` / `or_cardinality`（微秒级，不再需要逐 doc 驱动）。
  - AND/OR 迭代 = croaring 物化 and/or + 结果迭代，或多视图 merge-iterate——bench 定夺。
  - skew probe = `view.contains`（8–28ns/次），SKEW_RATIO 用 croaring contains 成本**重新标定**。
  - 档 2 物化低 df 子句逻辑不变（`for_each_doc`），对 bitmap 子句用 view.contains 过滤。
- **删除**：自研 `roaring.rs` 容器库（Container/RoaringBitmap/RoaringCursor/serialize/
  deserialize/from_sorted_docs/and/or/optimize）、`roaring/simd.rs`（AVX2 内核）、
  `roaring/view.rs`（M4 RoaringView）。保留：magic/version/df/cardinality 头 + len 尾缀 +
  `locate_bitmap_region` 定位 + BITMAP_MIN_DF 门槛。
- **明确不做**：multi-term roaring 集成（仍二期）；跨查询缓存；Java portable 格式（payload
  Rust 私有，frozen 是 CRoaring 私有格式无碍）；NRT。

## 3. 格式 v3

```
[ magic(4B "RLBM") + version(1B) = 3 + df(vInt) + cardinality(vInt) + Frozen payload ][ len: u32 LE ]
```

- payload = CRoaring Frozen 格式（`roaring_bitmap_frozen_size_in_bytes` 给精确尺寸；len 上界
  公式按计划阶段对 croaring-sys 源码推导的值）。
- `version != 3` → `Ok(None)` 落档 postings；v1/v2 索引零迁移（同 M4 的版本门纪律）。
- 写侧仍经同一 ChecksumIndexOutput（footer CRC 覆盖）；对齐不在写侧保证（.doc 流内偏移
  任意），由读侧对齐 buffer 解决。

## 4. unsafe 与依赖边界

- 新增依赖：`croaring = "2.7"`（croaring-sys 4.7.1 内嵌 CRoaring C 源码编译，无系统库依赖；
  release 构建 +11s；musl target 实测通过）。
- `BitmapView::deserialize::<Frozen>` 是唯一 unsafe 调用点（32B 对齐 + 精确长度契约由调用方
  保证）——codec-lucene9 第三个模块级 `#[allow(unsafe_code)]`；core 保持 `#![forbid(unsafe_code)]`，
  core 只调 codec 暴露的安全包装。
- 自研 AVX2 内核删除后，`postings_ll/simd.rs` 恢复为唯一既有 allow 模块（加新 view 模块共两个）。

## 5. 验证

- **Rust 对拍**：bitmap on/off 结果逐位一致；**v2 落档测试**（M4 写出的 v2 索引在 v3 读侧
  静默落 postings、结果与 PFOR 一致，沿用 M4 的 doctoring 手法）。
- **Java 兼容终验**：`make log-test` 五变体全绿（CheckIndex + forceMerge + A/B）。
- **bench 复测**（M3/M4 同口径 + skew 重标定）：预期稀疏 AND ≥2x PFOR（7.7µs 级 count +
  物化迭代）、稠密 ≥8x、term count ~1.0x、iterm ≥1x；写侧 `Bitmap::of` 开销量化。
  报告落 `.superpowers/sdd/m5-bench-report.md`（gitignored）。

## 6. 任务切分与工作量

1. 依赖引入 + 格式 v3 写侧（croaring 构建 + Frozen payload + len 上界推导 + v2 落档测试）。
2. 读侧 Frozen view 打开（对齐 buffer + unsafe 封装 + contains/iter 安全 API）+ Term 接入。
3. AND/OR 引擎（cardinality 快路径 + 物化迭代 + 档 2 contains 过滤 + SKEW_RATIO 重标定）。
4. 删除自研库（roaring.rs 容器/simd/view）+ 全仓引用清零 + 电池复跑。
5. bench 复测 + 新旧对比报告。

估 ~600 行净改动（删除为主）。croaring API 用法参照 `/tmp/croaring-probe/src/main.rs`。
