### Task 10: 报告 §12 + Phase 2 决策备忘

**Files:**
- Modify: `docs/bool-bench-report.md`（追加 §12）

**Interfaces:**
- Consumes: Task 8/9 全部数据文件
- Produces: 报告 §12（5M 方法论 + block on/off 表 + 1M↔5M 标度 + Phase 2 决策）；spec §6.5 交付完成

- [ ] **Step 1: 汇总关键数字**

从 `/tmp/bench-5m-summary.txt` + Task 8 笔记提取：每桶 p50 × 15 cells；hits × 桶（5M 规模）；PFOR block on/off 提速倍数；perf 数据（或降级说明）。

- [ ] **Step 2: 写 §12**

在 `docs/bool-bench-report.md` 尾部追加 §12，结构：

```markdown
## 12. 批量迭代（块级 DocIter）——5M 单段 bench（2026-07-xx）

### 12.1 方法论
- 索引：logwrite 5M seed=42 --bitmap + forcemerge --bitmap → 单段 5,000,000 docs / ≈X GB
- 查询集：/tmp/boolq.txt 715 条复用（df 等比 ×5，level 词 df≈1M，选择率 ~20% 不变）
- 矩阵：roaring/pfor × block on/off × 三模式 + Java 基线；warmup 3 / iter 10
- 引擎改动：spec 2026-07-26（commit 链）；RL_BLOCK=0 逃生门逐 query 对账 0 差异
- perf 证据等级：[PMU 数据 | 墙钟+软件火焰图降级说明]

### 12.2 block on/off 对照（no-fast 全量迭代，p50 µs）
[表格：形状桶 × {roaring on, roaring off, 倍数, pfor on, pfor off, 倍数, java}]

### 12.3 count / topN 模式
[同构两表]

### 12.4 1M↔5M 标度
[重点桶 1M→5M 放大系数 vs 线性 5× 的偏离分析]

### 12.5 与 demo 预估对照
demo 预估 PFOR 3-10× / roaring 1.5-4×（spec §1.2）→ 实测 [X]；差异归因。

### 12.6 Phase 2 决策备忘
- profile 热点：[代数内核占比 / 解码占比 / collect 占比]
- 决策：[做/不做 SIMD；做哪几个内核；理由]
- 设计偏差记录：BitsetDocIter 默认 fill（Task 3）实测影响 [有/无]

### 12.7 复现命令
[Task 9 命令精简版]
```

注：数据缺失处用实测填充，勿留占位符；跑数日期写实际日期。

- [ ] **Step 3: 提交报告**

```bash
git add docs/bool-bench-report.md
git commit -m "$(cat <<'EOF'
docs: bool bench 报告 §12——5M 单段批量迭代 bench 结果 + Phase 2 决策备忘

spec 2026-07-26 收口：block on/off × 三路三模式对照、1M↔5M 标度、
demo 预估对照、RL_BLOCK 逃生门 0 差异证据。

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 4: 收尾检查**

Run:
```bash
cargo test --workspace 2>&1 | tail -3
git log --oneline -12
git status --short
```
Expected: 测试全绿；commit 链 Task 1-7 + 10 完整；工作区仅剩有意排除项（`.cargo/config.toml`、`rust-toolchain.toml`——历史遗留，不属本计划）

---

## Self-Review 记录

**Spec 覆盖**：§0 硬约束 → Task 8（约束 1/3）、Task 9 Step 6/8（约束 2/4）；§2 架构 → Task 1/3/5/6；§3 trait 形状 → Task 1（+ DocBlockBuf::new 构造器补强）；§4 代数 → Task 4/5/6（Bitset 偏差 Task 3 记录，RoaringAnd probes 过滤 Task 6）；§5 driver → Task 1/7；§6 bench → Task 9/10；§7 电池 → Task 1-7 单测 + Task 8/9 集成；§8 排序 → Task 1→10 一致；§9 风险 → Task 8 Step 5（低 df 桶监视）、Task 9 Step 1（磁盘）、Task 9 Step 9（PMU 降级）；§10 Phase 2 → Task 10 §12.6。

**已知实现期决策点**（非占位符，已在任务内注明解法）：Task 6 Step 7 leaves_for_test 暴露方式；Task 6 借用冲突（heads/consume 时序、curs/sub split）解法已内嵌代码。

**自审修正记录**：(1) n 元合取初版的 scratch 左折叠有消费坐标 bug（第 2 轮起 `block_intersect` 的 ca 是 scratch 坐标而非 child0 块坐标，游标会欠消费）——已改为"二元 slice 快路径 + n 元逐元素 `position_*` 窗口定位"，正确性由前向单调性保证；(2) spec §4 的 (1)-(3) 不变量由对拍测试（40 轮随机 + 四象限 + searcher 级多段）差分执行，不加运行时 debug_assert（跨调用单调性断言要给 trait 加状态字段，YAGNI）；(3) `block_intersect` 的 Phase 1 消费方 = 三处二元快路径（and 两子句是 bench 主力形状）。
