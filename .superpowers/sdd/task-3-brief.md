### Task 3: 叶子覆写——MatchAll / Bitmap 游标 / Docs / Freqs

**Files:**
- Modify: `crates/core/src/search/doc_iter.rs`（MatchAllIter :54-78；BitmapCursor :640-682；RoaringDocIter impl :702-727；MaterializedDocIter impl :750-775；SegmentDocIter::next_block 的 Docs/Freqs 臂——Task 1 内联版换批读）
- Modify: `crates/core/src/search/block_tests.rs`（追加叶子对拍）

**Interfaces:**
- Consumes: Task 2 的 `DocsEnum::next_docs` / `DocsFreqsEnum::{next_docs_and_freqs, decodes_freqs}`；`BitmapCursor` 私有字段（buf/pos/end/refill——同文件内可访问）
- Produces: 叶子 `next_block` 覆写（RoaringDocIter / MaterializedDocIter 经 `BitmapCursor::next_many_to`；MatchAllIter 算术填充；SegmentDocIter Docs/Freqs 臂批读）；`BitmapCursor::next_many_to`（Task 6 的 DocSource 复用）

**设计偏差记录**：`BitsetDocIter` 不加专用覆写——`FixedBitSet` 无公开 word 访问（bitset.rs :7-60 只有 next_set_bit/get/popcount），而 next_set_bit 本就是 word 级 trailing_zeros 扫描，专用覆写无增量收益；保持默认 fill。若 Phase 1 profile 证实 bitset 路径热点再议（记入报告 §12）。

- [ ] **Step 1: 写失败测试——叶子对拍**

`block_tests.rs` 尾部追加：

```rust
use super::doc_iter::{MatchAllIter, RoaringDocIter};

#[test]
fn matchall_block_stream() {
    for max in [0i32, 1, 127, 128, 129, 300, 1000] {
        let mut it = SegmentDocIter::All(MatchAllIter::new(max));
        let expect: Vec<u32> = (0..max as u32).collect();
        assert_eq!(stream_block(&mut it), expect, "max={max}");
    }
}

#[test]
fn leaves_block_vs_per_doc_random() {
    let mut lcg = Lcg(7);
    for _ in 0..20 {
        let n = (lcg.next_u32() % 600) as usize;
        let docs = lcg.doc_set(50_000, n);
        // Materialized 叶子
        let mut a = leaf(&docs);
        let mut b = leaf(&docs);
        assert_eq!(stream_block(&mut a), stream_per_doc(&mut b), "mat n={n}");
    }
}
```

注：`RoaringDocIter` 需要 `FrozenBitmap`（codec 内部视图，测试不易构造）——其覆写与 Materialized 共用 `BitmapCursor::next_many_to`，Materialized 对拍即覆盖游标批读逻辑；Roaring 路径由 Task 8 的 1M 电池（roaring 引擎全形状逐 query 对账）终验。import 行若编译器报未使用则删 `RoaringDocIter`。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p rustlucene-core matchall_block 2>&1 | tail -5`
Expected: FAIL——MatchAll 走默认 fill 时 `stream_block` 其实已正确（默认 fill 对 MatchAll 语义正确）→ 此测试可能直接 PASS；`leaves_block_vs_per_doc_random` 同理 PASS（默认 fill 正确）。**本任务测试是回归守卫而非红灯起步**：确认当前 PASS 后继续（覆写只改性能不改行为）。

- [ ] **Step 3: BitmapCursor::next_many_to**

在 `impl<B: DocsBitmap> BitmapCursor<B>`（:640-682）的 `advance` 方法之后追加：

```rust
    /// 批量产出（spec 2026-07-26 Task 3）：把缓冲与后续 refill 的 doc
    /// 拷入 dst 至填满或耗尽，返回产出数。维护 next_from 不变量
    /// （= 最后产出 doc + 1），保证批读后 advance() 语义不变。
    /// docs < max_doc <= i32::MAX，+1 不溢出。
    fn next_many_to(&mut self, dst: &mut [u32]) -> usize {
        let mut n = 0;
        while n < dst.len() {
            if self.pos >= self.end && !self.refill() {
                break;
            }
            let take = (self.end - self.pos).min(dst.len() - n);
            dst[n..n + take].copy_from_slice(&self.buf[self.pos..self.pos + take]);
            self.pos += take;
            n += take;
        }
        if n > 0 {
            self.next_from = dst[n - 1] + 1;
        }
        n
    }
```

- [ ] **Step 4: RoaringDocIter / MaterializedDocIter 覆写（共享 helper）**

两者共用 `BitmapCursor<B: DocsBitmap>` 游标且语义逐字相同——抽一个自由函数（放在 `BitmapCursor` impl 块之后的 bitmap 迭代器区），两个 impl 各调一行（评审裁决 2026-07-26：原"复制勿抽公共"指示撤销）：

```rust
/// BitmapCursor 叶子共用的块产出（RoaringDocIter / MaterializedDocIter）：
/// 游标批读 + doc 游标簿记。耗尽后 doc 钉 NO_MORE_DOCS。
fn cursor_next_block<B: DocsBitmap>(
    cur: &mut BitmapCursor<B>,
    doc: &mut i32,
    out: &mut DocBlockBuf,
) -> io::Result<usize> {
    if *doc == NO_MORE_DOCS {
        out.len = 0;
        return Ok(0);
    }
    let n = cur.next_many_to(&mut out.docs);
    *doc = if n == 0 { NO_MORE_DOCS } else { out.docs[n - 1] as i32 };
    out.len = n;
    Ok(n)
}
```

在 `impl DocIter for RoaringDocIter`（:702-727）的 `advance` 之后追加：

```rust
    fn next_block(&mut self, out: &mut DocBlockBuf) -> io::Result<usize> {
        cursor_next_block(&mut self.cur, &mut self.doc, out)
    }
```

`impl DocIter for MaterializedDocIter`（:750-775）追加逐字相同的三行覆写。

- [ ] **Step 5: MatchAllIter 覆写**

`impl DocIter for MatchAllIter`（:54-78）的 `advance` 之后追加：

```rust
    fn next_block(&mut self, out: &mut DocBlockBuf) -> io::Result<usize> {
        if self.doc == NO_MORE_DOCS {
            out.len = 0;
            return Ok(0);
        }
        let start = (self.doc + 1).max(0) as u32;
        let n = (self.max_doc as u32).saturating_sub(start).min(DOC_BLOCK as u32) as usize;
        for i in 0..n {
            out.docs[i] = start + i as u32;
        }
        self.doc = if n == 0 || start + n as u32 >= self.max_doc as u32 {
            NO_MORE_DOCS
        } else {
            (start + n as u32 - 1) as i32
        };
        out.len = n;
        Ok(n)
    }
```

- [ ] **Step 6: SegmentDocIter Docs/Freqs 臂换 codec 批读**

替换 Task 1 在 `SegmentDocIter::next_block` 里写的两个内联填充臂：

```rust
            Self::Docs(d) => {
                let n = d.next_docs(&mut out.docs)?;
                out.len = n;
                Ok(n)
            }
            Self::Freqs(f) => {
                let n = if f.decodes_freqs() {
                    f.next_docs_and_freqs(&mut out.docs, &mut out.freqs)?
                } else {
                    f.next_docs(&mut out.docs)?
                };
                out.len = n;
                Ok(n)
            }
```

- [ ] **Step 7: 跑测试确认通过**

Run: `cargo test -p rustlucene-core 2>&1 | tail -3`
Expected: 全绿（277 + 2 = 279 passed）

- [ ] **Step 8: Commit**

```bash
git add crates/core/src/search/doc_iter.rs crates/core/src/search/block_tests.rs
git commit -m "$(cat <<'EOF'
feat(batch): 叶子 next_block 覆写——bitmap 游标 next_many_to / MatchAll 算术填充 / Docs-Freqs codec 批读

spec 2026-07-26 Task 3：叶子批读 = 内部缓冲直拷，零新增解码。
BitsetDocIter 保持默认 fill（FixedBitSet 无 word 公开访问，
next_set_bit 已 word 级，记入报告 §12 设计偏差）。

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```

---

