### Task 8: 1M 回归电池（block ON 零回归 + RL_BLOCK=0 逐位一致）

**Files:**
- 产出（不入库）：`/tmp/regress-1m-*.tsv`
- Modify（仅当发现回归时）：对应引擎文件

**Interfaces:**
- Consumes: 现有 `/tmp/boolbench-idx`（1M，2 段）、`/tmp/boolq.txt`（715 条）、报告附录复现命令
- Produces: 回归结论文本（贴入 Task 10 报告 §12 附录）；P1-1/P1-3 重点行零回归证据（spec §0 硬约束 3）

- [ ] **Step 1: release 构建**

Run: `cargo build --release 2>&1 | tail -2`
Expected: 构建成功

- [ ] **Step 2: block ON × roaring × 三模式**

Run:
```bash
cd /home/yjw/lucene-rust
./target/release/rustlucene-cli searchbench /tmp/boolbench-idx message \
  --load-queries /tmp/boolq.txt --warmup 10 --iter 30 > /tmp/regress-1m-roaring.tsv 2>&1
./target/release/rustlucene-cli searchbench /tmp/boolbench-idx message \
  --load-queries /tmp/boolq.txt --warmup 10 --iter 30 --no-fast-count > /tmp/regress-1m-roaring-iter.tsv 2>&1
./target/release/rustlucene-cli searchbench /tmp/boolbench-idx message \
  --load-queries /tmp/boolq.txt --warmup 10 --iter 30 --topn 10 > /tmp/regress-1m-roaring-topn.tsv 2>&1
```
Expected: 三个 TSV 生成，无报错

- [ ] **Step 3: block OFF × roaring × no-fast（逃生门逐位一致验证）**

Run:
```bash
RL_BLOCK=0 ./target/release/rustlucene-cli searchbench /tmp/boolbench-idx message \
  --load-queries /tmp/boolq.txt --warmup 10 --iter 30 --no-fast-count > /tmp/regress-1m-roaring-iter-noblock.tsv 2>&1
```

逐 query hits diff（必须为空——spec 硬约束 1）：
```bash
diff <(grep 'bucket=' /tmp/regress-1m-roaring-iter.tsv | awk -F'\t' '{print $1"\t"$3}' | sort) \
     <(grep 'bucket=' /tmp/regress-1m-roaring-iter-noblock.tsv | awk -F'\t' '{print $1"\t"$3}' | sort)
```
Expected: 无输出（0 差异）

- [ ] **Step 4: PFOR 路径 block ON/OFF 对账**

Run:
```bash
RL_BITMAP=0 ./target/release/rustlucene-cli searchbench /tmp/boolbench-idx message \
  --load-queries /tmp/boolq.txt --warmup 10 --iter 30 --no-fast-count > /tmp/regress-1m-pfor-iter.tsv 2>&1
RL_BITMAP=0 RL_BLOCK=0 ./target/release/rustlucene-cli searchbench /tmp/boolbench-idx message \
  --load-queries /tmp/boolq.txt --warmup 10 --iter 30 --no-fast-count > /tmp/regress-1m-pfor-iter-noblock.tsv 2>&1
diff <(grep 'bucket=' /tmp/regress-1m-pfor-iter.tsv | awk -F'\t' '{print $1"\t"$3}' | sort) \
     <(grep 'bucket=' /tmp/regress-1m-pfor-iter-noblock.tsv | awk -F'\t' '{print $1"\t"$3}' | sort)
```
Expected: 无输出

- [ ] **Step 5: P1-1/P1-3 重点行性能比对**

提取重点桶 p50（与报告 §9/§10 基线 c51e27b 比，±10% 内）：

```bash
for f in /tmp/regress-1m-roaring-iter.tsv /tmp/bench-rust-roaring-iter.tsv; do
  echo "== $f"; grep -E "^(mnfhh|multinot|nothi|rngmust|rngnot|bool|orcross)" "$f" | awk -F'\t' '{print $1, $2, $4}'
done
```

Expected: block ON 新跑数 vs `/tmp/bench-rust-roaring-iter.tsv`（c51e27b 基线）：mnfhh/mnfmh/multinot/nothi/rngmust/rngnot 各桶 p50 变化 ±10% 内（roaring 路径本就走 materialize fold，块化影响应近零）；PFOR 桶（`/tmp/regress-1m-pfor-iter.tsv` vs `/tmp/bench-rust-pfor-iter.tsv`）预期**下降**（块化收益）——记录倍数，Task 10 报告引用。

- [ ] **Step 6: count 模式逐 query 对账**

```bash
diff <(grep 'bucket=' /tmp/regress-1m-roaring.tsv | awk -F'\t' '{print $1"\t"$3}' | sort) \
     <(grep 'bucket=' /tmp/bench-rust-roaring.tsv | awk -F'\t' '{print $1"\t"$3}' | sort)
```
Expected: 无输出

- [ ] **Step 7: 记录结论 + Commit（仅数据记录，无代码改动则跳过 commit）**

把 Step 3-6 的 diff 结果与 Step 5 对照表写入临时笔记 `/tmp/regress-1m-result.txt`（Task 10 报告引用）。若本任务触发任何代码修复，单独 commit 并在 message 注明回归修复。

---

