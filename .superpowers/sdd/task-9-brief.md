### Task 9: 5M 单段索引 + 15-cell 矩阵 + 对账 + perf

**Files:**
- 产出（不入库）：`/tmp/boolbench-5m/`（索引）、`/tmp/bench-5m-*.tsv`、`/tmp/bench-5m-*.txt`（Java detail）
- 前置检查：`df -h /tmp`（≥5GB 空闲）

**Interfaces:**
- Consumes: 全部引擎改动（Task 1-7）；`logwrite`/`forcemerge`/`searchbench` CLI；`interop/java/classes` 的 SearchBench
- Produces: 5M 三路三模式 × block on/off 原始数据（Task 10 报告输入）；5M 四路对账（spec §7-3）

- [ ] **Step 1: 磁盘检查 + 构建 5M 索引**

Run:
```bash
df -h /tmp | tail -1
cd /home/yjw/lucene-rust
time ./target/release/rustlucene-cli logwrite /tmp/boolbench-5m 5000000 42 --bitmap
```
Expected: 写入成功（预计 2-5 分钟）；多段产生（默认 flush 护栏）

- [ ] **Step 2: forcemerge 成单段（必带 --bitmap）**

Run:
```bash
time ./target/release/rustlucene-cli forcemerge /tmp/boolbench-5m --bitmap
ls /tmp/boolbench-5m/*.si
```
Expected: 单个 `_N.si`（spec §6.1 验收）。`--bitmap` 必带——否则合并段无 RLBM 内联 bitmap，roaring 口径失真。

- [ ] **Step 3: 索引验收（maxDoc / delCount / RLBM 存在）**

Run:
```bash
./target/release/rustlucene-cli searchbench /tmp/boolbench-5m message \
  --load-queries /tmp/boolq.txt --warmup 1 --iter 1 2>&1 | head -3
ls /tmp/boolbench-5m/ | grep -c RLBM
du -sh /tmp/boolbench-5m
```
Expected: searchbench 头部显示 1 segment / maxDoc=5000000（若 CLI 不打印段数，用 `ls *.si | wc -l` = 1 验证）；RLBM 文件存在（df≥4096 词有内联 bitmap）；体积 ≈1.1-1.2GB

- [ ] **Step 4: Rust roaring × 三模式 × block on/off（6 cells）**

Run:
```bash
B=/home/yjw/lucene-rust/target/release/rustlucene-cli
for mode in "" "--no-fast-count" "--topn 10"; do
  suf=$(echo "$mode" | sed 's/--no-fast-count/iter/; s/--topn 10/topn/; s/ //')
  $B searchbench /tmp/boolbench-5m message --load-queries /tmp/boolq.txt \
    --warmup 3 --iter 10 $mode > /tmp/bench-5m-roaring${suf:+-$suf}.tsv 2>&1
  RL_BLOCK=0 $B searchbench /tmp/boolbench-5m message --load-queries /tmp/boolq.txt \
    --warmup 3 --iter 10 $mode > /tmp/bench-5m-roaring${suf:+-$suf}-noblock.tsv 2>&1
done
```
Expected: 6 个 TSV 生成

- [ ] **Step 5: Rust PFOR × 三模式 × block on/off（6 cells）**

Run: 同 Step 4，前置 `RL_BITMAP=0`（输出文件名 `bench-5m-pfor-*`）：
```bash
for mode in "" "--no-fast-count" "--topn 10"; do
  suf=$(echo "$mode" | sed 's/--no-fast-count/iter/; s/--topn 10/topn/; s/ //')
  RL_BITMAP=0 $B searchbench /tmp/boolbench-5m message --load-queries /tmp/boolq.txt \
    --warmup 3 --iter 10 $mode > /tmp/bench-5m-pfor${suf:+-$suf}.tsv 2>&1
  RL_BITMAP=0 RL_BLOCK=0 $B searchbench /tmp/boolbench-5m message --load-queries /tmp/boolq.txt \
    --warmup 3 --iter 10 $mode > /tmp/bench-5m-pfor${suf:+-$suf}-noblock.tsv 2>&1
done
```

- [ ] **Step 6: Rust 块开/关逐 query 对账（必须 0 差异 × 12 文件对）**

Run:
```bash
for base in roaring roaring-iter roaring-topn pfor pfor-iter pfor-topn; do
  echo "== $base"
  diff <(grep 'bucket=' /tmp/bench-5m-$base.tsv | awk -F'\t' '{print $1"\t"$3}' | sort) \
       <(grep 'bucket=' /tmp/bench-5m-$base-noblock.tsv | awk -F'\t' '{print $1"\t"$3}' | sort) | head -5
done
```
Expected: 六个 `==` 行全部无 diff 输出（spec 硬约束 2）

- [ ] **Step 7: Java × 三模式（3 cells）**

Run:
```bash
CP=/home/yjw/lucene-rust/interop/java/classes:$(ls /home/yjw/lucene-rust/interop/java/lucene-*.jar | tr '\n' ':')
JB=/home/yjw/lucene-rust/interop/java
for mode in "" "--no-fast-count" "--topn 10"; do
  suf=$(echo "$mode" | sed 's/--no-fast-count/iter/; s/--topn 10/topn/; s/ //')
  java -Xmx2g -cp "$CP" SearchBench /tmp/boolbench-5m message \
    --load-queries /tmp/boolq.txt --no-cache --warmup 3 --iter 10 $mode \
    > /tmp/bench-5m-java${suf:+-$suf}.tsv 2> /tmp/bench-5m-java${suf:+-$suf}-detail.txt
done
```
Expected: 3 TSV + 3 detail 文件；Java 堆 5M 索引给 2g（1M 用 512m，线性放大）

- [ ] **Step 8: Rust↔Java 逐 query 对账（排除已知 term 采样差异）**

Run:
```bash
for suf in "" "-iter" "-topn"; do
  echo "== mode$suf"
  diff <(grep 'bucket=' /tmp/bench-5m-roaring$suf.tsv | awk -F'\t' '{print $1"\t"$3}' | sort) \
       <(grep 'bucket=' /tmp/bench-5m-java$suf-detail.txt | awk -F'\t' '{print $1"\t"$3}' | sort) \
    | grep -v '^term[s]*=' | head -10
done
```
Expected: 过滤 term 桶后 0 差异（term 差异 = Java 采样 count 已知行为，报告 §7 记录）。有非 term 差异 → 停，排查（正确性优先于 bench）。

- [ ] **Step 9: perf 尝试（PMU 可用时；否则记录降级）**

Run:
```bash
perf stat -e cycles,instructions,branches,branch-misses \
  $B searchbench /tmp/boolbench-5m message --load-queries /tmp/boolq.txt \
  --warmup 1 --iter 5 --no-fast-count 2>&1 | grep -E "cycles|instructions|branch|elapsed"
```
Expected: 若输出 `<not supported>` → 记录"bench 机 PMU 不可用，证据等级 = 墙钟 + 软件火焰图"（spec §6.4），改跑：
```bash
perf record -e cpu-clock -g -o /tmp/perf-5m-block.data -- \
  $B searchbench /tmp/boolbench-5m message --load-queries /tmp/boolq.txt --warmup 1 --iter 5 --no-fast-count
RL_BLOCK=0 perf record -e cpu-clock -g -o /tmp/perf-5m-noblock.data -- \
  $B searchbench /tmp/boolbench-5m message --load-queries /tmp/boolq.txt --warmup 1 --iter 5 --no-fast-count
perf report -i /tmp/perf-5m-block.data --stdio | head -30 > /tmp/perf-5m-block-top.txt
perf report -i /tmp/perf-5m-noblock.data --stdio | head -30 > /tmp/perf-5m-noblock-top.txt
```
PMU 可用时：PFOR 热点桶（`RL_BITMAP=0`，no-fast）block on/off 各跑 `perf stat`，记录 branches/branch-misses 差。

- [ ] **Step 10: 汇总数据摘要**

Run:
```bash
for f in /tmp/bench-5m-*.tsv; do echo "== $f"; head -12 "$f" | tail -10; done > /tmp/bench-5m-summary.txt
```
Expected: 摘要文件生成，供 Task 10 报告引用

---

