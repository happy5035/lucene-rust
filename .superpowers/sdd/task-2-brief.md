### Task 2: codec 窗口批读——EnumCore::next_docs

**Files:**
- Modify: `crates/codec-lucene9/src/postings_read.rs`（EnumCore impl :245-630 区间；DocsEnum :656-672；DocsFreqsEnum :675-698；tests mod :791+）

**Interfaces:**
- Consumes: `EnumCore` 私有状态（`doc_buffer: [u64; BLOCK_SIZE+1]`、`doc_buffer_upto`、`freq_buffer`、`level0_last_doc`、`move_to_next_level0_block()`、NO_MORE_DOCS 哨兵）
- Produces: `EnumCore::next_docs`、`DocsEnum::next_docs`、`DocsFreqsEnum::{next_docs, next_docs_and_freqs, decodes_freqs}`——Task 3 的 `SegmentDocIter::Docs/Freqs` 覆写依赖这些

- [ ] **Step 1: 写失败测试——codec 批读对拍**

在 `postings_read.rs` 的 `mod tests` 尾部追加（helper `write_segment`/`seek`/`temp_dir` 已存在 :800-860）：

```rust
    /// 批读 vs 逐 doc 全量对拍：kw:big（df=200 稠密）/ kw:tail（df=3 尾块）/
    /// tx:hot（df=5000，跨 level-1 边界 4096）/ tx:warm（df=200 步长3 +
    /// freq 异常值）/ tx:one（singleton）。多种 dst 尺寸含 1（退化）与
    /// 4096（超 level-1 组）。
    fn drain_next_docs(en: &mut DocsEnum, step: usize) -> Vec<u32> {
        let mut docs = Vec::new();
        let mut buf = vec![0u32; step];
        loop {
            let n = en.next_docs(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            docs.extend_from_slice(&buf[..n]);
        }
        docs
    }

    fn drain_per_doc(en: &mut DocsEnum) -> Vec<u32> {
        let mut docs = Vec::new();
        loop {
            let d = en.next_doc().unwrap();
            if d == NO_MORE_DOCS {
                break;
            }
            docs.push(d as u32);
        }
        docs
    }

    #[test]
    fn next_docs_matches_next_doc_all_terms() {
        let dir = temp_dir("nextdocs");
        fs::create_dir_all(&dir).unwrap();
        let fsdir = FSDirectory::open(&dir).unwrap();
        let (fis, warm_docs, warm_freqs) = write_segment(&fsdir);
        let reader = PostingsReader::open(&fsdir, "_0", &[4u8; 16]).unwrap();

        // (field, term, expect_docs, expect_freqs)
        let big: Vec<u32> = (0..200).collect();
        let hot: Vec<u32> = (0..5000).collect();
        let cases: Vec<(&str, &[u8], Vec<u32>, Option<Vec<u32>>)> = vec![
            ("kw", b"big", big.clone(), None),
            ("kw", b"tail", vec![10, 20, 30], None),
            ("tx", b"hot", hot, Some(vec![1; 5000])),
            ("tx", b"warm", warm_docs, Some(warm_freqs)),
            ("tx", b"one", vec![42], Some(vec![7])),
        ];
        for (field, term, expect_docs, expect_freqs) in cases {
            let entry = seek(&fsdir, &fis, field, term);
            for step in [1usize, 7, 128, 200, 4096] {
                let mut en = reader.docs(&entry).unwrap();
                assert_eq!(drain_next_docs(&mut en, step), expect_docs,
                    "{field}:{term:?} step={step} docs");
            }
            // 逐 doc 参照路径同集
            let mut en = reader.docs(&entry).unwrap();
            assert_eq!(drain_per_doc(&mut en), expect_docs);
            // freqs 对拍（仅 has_freqs 字段）
            if let Some(expect_f) = expect_freqs {
                let mut en = reader.docs_and_freqs(&entry).unwrap();
                assert!(en.decodes_freqs());
                let mut docs = Vec::new();
                let mut freqs = Vec::new();
                let (mut db, mut fb) = (vec![0u32; 64], vec![0u32; 64]);
                loop {
                    let n = en.next_docs_and_freqs(&mut db, &mut fb).unwrap();
                    if n == 0 {
                        break;
                    }
                    docs.extend_from_slice(&db[..n]);
                    freqs.extend_from_slice(&fb[..n]);
                }
                assert_eq!(docs, expect_docs, "{field}:{term:?} freq-mode docs");
                assert_eq!(freqs, expect_f, "{field}:{term:?} freqs");
            }
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_freq_enum_does_not_decode_freqs() {
        let dir = temp_dir("nofreqbatch");
        fs::create_dir_all(&dir).unwrap();
        let fsdir = FSDirectory::open(&dir).unwrap();
        let (fis, warm_docs, _) = write_segment(&fsdir);
        let reader = PostingsReader::open(&fsdir, "_0", &[4u8; 16]).unwrap();
        let entry = seek(&fsdir, &fis, "tx", b"warm");
        let mut en = reader.docs_and_freqs_no_freq(&entry).unwrap();
        assert!(!en.decodes_freqs());
        assert_eq!(drain_next_docs_enum(&mut en, 128), warm_docs);
        fs::remove_dir_all(&dir).unwrap();
    }

    fn drain_next_docs_enum(en: &mut DocsFreqsEnum, step: usize) -> Vec<u32> {
        let mut docs = Vec::new();
        let mut buf = vec![0u32; step];
        loop {
            let n = en.next_docs(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            docs.extend_from_slice(&buf[..n]);
        }
        docs
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p codec-lucene9 next_docs 2>&1 | tail -5`
Expected: 编译错误——`next_docs` / `decodes_freqs` / `next_docs_and_freqs` 未定义

- [ ] **Step 3: EnumCore::next_docs 实现**

在 `impl EnumCore` 块内 `next_doc` 方法（:295-309）之后插入：

```rust
    /// 批量版 next_doc（spec 2026-07-26 Task 2）：仅 docs / docs+freqs
    /// profile（pos.is_none()）——把 doc_buffer 里已解码的绝对 doc 窗口
    /// 直接拷出，跨 128-block 边界透明 refill。freqs = Some 时同步拷
    /// freq_buffer 同窗口（调用方保证 decode_freqs）。EverythingEnum 的
    /// position 簿记不走这里——PositionsEnum 保持逐 doc（phrase 两阶段）。
    /// 返回 0 = 耗尽。
    fn next_docs(&mut self, docs: &mut [u32], mut freqs: Option<&mut [u32]>) -> io::Result<usize> {
        debug_assert!(self.pos.is_none());
        let mut n = 0;
        while n < docs.len() {
            if self.doc == NO_MORE_DOCS as i64 {
                break;
            }
            if self.doc == self.level0_last_doc {
                self.move_to_next_level0_block()?;
            }
            let upto = self.doc_buffer_upto;
            // 窗口 = 当前缓冲到哨兵（NO_MORE_DOCS 占位）或 dst 填满
            let mut take = 0;
            while take < docs.len() - n
                && self.doc_buffer[upto + take] != NO_MORE_DOCS as u64
            {
                take += 1;
            }
            if take == 0 {
                // 缓冲首槽即哨兵（df 恰为 128 倍数后的空 refill）：
                // 镜像 next_doc 读哨兵一步，置耗尽态。
                self.doc = self.doc_buffer[upto] as i64;
                self.doc_buffer_upto = upto + 1;
                break;
            }
            for j in 0..take {
                docs[n + j] = self.doc_buffer[upto + j] as u32;
            }
            if let Some(f) = freqs.as_deref_mut() {
                for j in 0..take {
                    f[n + j] = self.freq_buffer[upto + j];
                }
            }
            self.doc_buffer_upto = upto + take;
            self.doc = self.doc_buffer[upto + take - 1] as i64;
            n += take;
        }
        Ok(n)
    }
```

- [ ] **Step 4: DocsEnum / DocsFreqsEnum 公开包装**

`impl DocsEnum`（:659-672）尾部追加：

```rust
    /// 批量产出已解码 doc（spec 2026-07-26）：0 = 耗尽。
    pub fn next_docs(&mut self, docs: &mut [u32]) -> io::Result<usize> {
        self.core.next_docs(docs, None)
    }
```

`impl DocsFreqsEnum`（:677-698）尾部追加：

```rust
    /// 批量产出 doc（不解 freq）：0 = 耗尽。
    pub fn next_docs(&mut self, docs: &mut [u32]) -> io::Result<usize> {
        self.core.next_docs(docs, None)
    }

    /// 批量产出 doc + freq 同窗口。调用方先查 decodes_freqs()——
    /// no-freq 模式（docs_and_freqs_no_freq 构造）下 freq_buffer 未
    /// 物化，调用即 panic（同 freq() 的 no-freq 契约）。
    pub fn next_docs_and_freqs(
        &mut self,
        docs: &mut [u32],
        freqs: &mut [u32],
    ) -> io::Result<usize> {
        assert!(self.core.decode_freqs, "next_docs_and_freqs on no-freq enum");
        self.core.next_docs(docs, Some(freqs))
    }

    /// freq 块是否实际解码（needs_freq 构造时为真）。
    pub fn decodes_freqs(&self) -> bool {
        self.core.decode_freqs
    }
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo test -p codec-lucene9 2>&1 | tail -3`
Expected: 全绿（185 + 2 = 187 passed）

- [ ] **Step 6: Commit**

```bash
git add crates/codec-lucene9/src/postings_read.rs
git commit -m "$(cat <<'EOF'
feat(codec): EnumCore::next_docs 窗口批读（128 解码块直拷，跨块透明 refill）

spec 2026-07-26 Task 2：DocsEnum::next_docs / DocsFreqsEnum::
next_docs_and_freqs + decodes_freqs。pos profile 不走批读（phrase 保持
逐 doc）。对拍：5 term × 5 dst 尺寸（1/7/128/200/4096）+ freq 异常值 +
singleton + no-freq 模式，与逐 doc 逐点一致。

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```

---

