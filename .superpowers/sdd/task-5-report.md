# Task 5 Report (WIP checkpoint): 组合器覆写（一）Excluding / ConjOver / DisjOver

**状态：未完成，交接给另一 agent。commit 532844a（WIP）。**

## 已完成（对拍绿）

- BlockCursor（Box<DocBlockBuf> + pos/len + new/refill/remaining/consume）
- position_seg 前向单调窗口定位（miss 不消费、hit 全员 consume(1)）
- ExcludingDocIter::next_block：prohibited 块级跳过（块 max < required 块 min 整块推进）
- ConjOverDocIter::next_block：k==2 二元快路（block_intersect，ca 直接 child0 坐标）+ k>2 position_seg 逐元素
- `combinator_edge_shapes` 绿（Excl/Conj 边界形状）

## 未完成（2 测试红）

- `block_tests::combinator_block_vs_per_doc_random`：失败在 "disj round=0"
- `block_tests::disj_debug_round0`：调试复现测试（40 轮随机第 0 轮固化，k=3，child 长度 69/403/447）

**症状**：DisjOverDocIter::next_block 产出流升序到某点（round0 为 1123）后，
某 child（稀疏的 child[0]，len=69）剩余块被整体先行产出到 3998，再回落到 1130
继续——伴随跨块重复（1312/1319/1360/1649/2098… 两次出现）。违反覆写契约 (2)(3)
（块内严格升序、跨块单调递增）。

**疑似根因**：kway_union 调用间的 heads/consumed 簿记。契约（T4 已钉）：
consumed 每轮调用前清零、heads 传各游标未消费余量 `&buf[pos..len]`。
乱序 + 重复的形态符合"refill 后某游标 heads 回卷/已消费区被重扫"。
排查从 DisjOverDocIter::next_block 的 refill 分支入手，参照
`algebra_consumed_coordinates_partial_fill` 的坐标语义。

## 接手清单

1. 修 DisjOverDocIter::next_block → `RUST_MIN_STACK=4194304 cargo test -p rustlucene-core block_` 全绿
2. 决定 disj_debug_round0 去留（保留为回归测试或并入随机测试）
3. 全工作区套件绿（284 + 本任务新增）
4. amend 或新 commit（subject 去掉 WIP 前缀），按 task-reviewer 模板走 Task 5 评审
5. T6-T10 按 plan 继续；**每个任务的 brief/report/review 一并提交**（.superpowers/sdd/ 已纳入版本控制）

## 测试数字（WIP 时刻）

codec 187 绿；core 95 绿 / 2 红 / 1 ignored。
