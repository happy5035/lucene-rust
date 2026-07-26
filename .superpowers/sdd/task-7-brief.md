### Task 7: top_docs 块路径 + TopDocCollector / FreqSumCollector 覆写

**Files:**
- Modify: `crates/core/src/search/searcher.rs`（top_docs :86-122）
- Modify: `crates/core/src/search/collector.rs`（TopDocCollector :47-54；FreqSumCollector :63-70）
- Modify: `crates/core/src/search/block_tests.rs`（collector 块语义测试）

**Interfaces:**
- Consumes: `drive_blocks`、叶子/组合器覆写（Task 1-6）
- Produces: 三 driver 全部块化；`TopDocCollector::collect_block`（total += len + 块前缀补满 n）、`FreqSumCollector::collect_block`（freqs 求和）

- [ ] **Step 1: 写失败测试——collector 块语义**

`block_tests.rs` 尾部追加：

```rust
use super::collector::{FreqSumCollector, TopDocCollector};

#[test]
fn top_doc_collector_block_per_doc_parity() {
    for top_n in [1usize, 5, 128, 129, 300, 1000] {
        let docs: Vec<u32> = (0..300).map(|i| i * 7).collect();
        // 逐 doc 参照
        let mut ref_c = TopDocCollector::new(top_n);
        for &d in &docs {
            ref_c.collect(d as i32, 1);
        }
        // 块路径（128 一块 + 尾块）
        let mut blk_c = TopDocCollector::new(top_n);
        for chunk in docs.chunks(128) {
            blk_c.collect_block(chunk, None);
        }
        assert_eq!(blk_c.total, ref_c.total, "top_n={top_n}");
        assert_eq!(blk_c.docs, ref_c.docs, "top_n={top_n}");
        assert_eq!(blk_c.docs.len(), top_n.min(docs.len()));
    }
}

#[test]
fn freq_sum_collector_block() {
    let mut c = FreqSumCollector::default();
    let docs = [1u32, 2, 3];
    c.collect_block(&docs, Some(&[4, 5, 6]));
    assert_eq!(c.total_freq, 15);
    let mut c2 = FreqSumCollector::default();
    c2.collect_block(&docs, None);
    assert_eq!(c2.total_freq, 3); // freq 恒 1
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p rustlucene-core top_doc_collector 2>&1 | tail -5`
Expected: 编译错误——`TopDocCollector::collect_block` / `FreqSumCollector::collect_block` 未定义（默认回退存在但测试需要覆写？——默认回退语义正确，测试可能 PASS；确认 PASS 后把覆写当性能改动加，测试作回归守卫）

- [ ] **Step 3: collector 覆写**

`impl Collector for TopDocCollector`（collector.rs :47-54）内 `collect` 之后追加：

```rust
    fn collect_block(&mut self, docs: &[u32], _freqs: Option<&[u32]>) {
        self.total += docs.len() as u64;
        let room = self.top_n.saturating_sub(self.docs.len());
        let take = room.min(docs.len());
        self.docs.extend(docs[..take].iter().map(|&d| d as i32));
    }
```

`impl Collector for FreqSumCollector`（:63-70）内追加：

```rust
    fn collect_block(&mut self, _docs: &[u32], freqs: Option<&[u32]>) {
        match freqs {
            Some(f) => self.total_freq += f.iter().map(|&x| x as u64).sum::<u64>(),
            None => self.total_freq += _docs.len() as u64,
        }
    }
```

- [ ] **Step 4: top_docs 块路径**

`top_docs` 方法（searcher.rs :86-122）的迭代段循环改为（保留 fast-count 短路语义逐条）：

```rust
    pub fn top_docs(&mut self, query: &Query, n: usize) -> io::Result<(u64, Vec<i32>)> {
        let mut total = 0u64;
        let mut docs: Vec<i32> = Vec::with_capacity(n.min(1024));
        for (doc_base, seg) in self.reader.leaves() {
            let fast = query::fast_segment_count(seg, query)?;
            if let Some(c) = fast {
                total += c;
            }
            if docs.len() >= n && fast.is_some() {
                continue;
            }
            let Some(mut iter) = query.segment_iterator(seg, false)? else {
                continue;
            };
            if block_enabled() {
                let mut out = DocBlockBuf::new();
                loop {
                    if docs.len() >= n && fast.is_some() {
                        break;
                    }
                    let cnt = iter.next_block(&mut out)?;
                    if cnt == 0 {
                        break;
                    }
                    if fast.is_none() {
                        total += cnt as u64;
                    }
                    for &d in &out.docs[..cnt] {
                        if docs.len() >= n {
                            break;
                        }
                        docs.push(doc_base + d as i32);
                    }
                }
                continue;
            }
            loop {
                if docs.len() >= n && fast.is_some() {
                    break;
                }
                let doc = iter.next_doc()?;
                if doc == NO_MORE_DOCS {
                    break;
                }
                if !iter.matches()? {
                    continue;
                }
                if fast.is_none() {
                    total += 1;
                }
                if docs.len() < n {
                    docs.push(doc_base + doc);
                }
            }
        }
        Ok((total, docs))
    }
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo test --workspace 2>&1 | tail -3`
Expected: 全绿（基线 + 本计划新增全部，0 failed）

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/search/searcher.rs crates/core/src/search/collector.rs crates/core/src/search/block_tests.rs
git commit -m "$(cat <<'EOF'
feat(batch): top_docs 块路径 + TopDocCollector/FreqSumCollector 块覆写

spec 2026-07-26 Task 7：INDEXORDER 块前缀补满 n + fast-count 短路逐条
保留；三 driver 全部块化完成。collector 块/逐 doc 对拍（top_n 跨块
边界 1/5/128/129/300/1000）。

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```

---

