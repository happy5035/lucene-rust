### Task 5: DocWriter 索引侧接入

**Files:**
- Modify: `crates/core/src/doc_writer.rs`（FieldBuf 加 analyzer、写入热路径分支）

**Interfaces:**
- Consumes: Task 2 `Analyzer::analyze`、Task 4 `FieldSpec.analyzer`
- Produces: 带 analyzer 的 text 字段写入即归一化（`ERROR`/`error` 归一到同一 term）；无 analyzer 字段字节级不变

- [ ] **Step 1: Write the failing test**

`crates/core/src/doc_writer.rs` 测试模块（line 687 起，与现有 `builds_postings_with_freqs_and_positions` 同写法）追加：

```rust
    #[test]
    fn analyzer_normalizes_terms_at_index_time() {
        let mut schema = Schema::new();
        schema.add(FieldSpec::text("message").with_analyzer("whitespace|lowercase"));
        let mut dw = DocWriter::new();
        let mut d0 = Document::new();
        d0.add("message", FieldValue::Text("ERROR Failed error".to_string()));
        dw.add_document(&schema, d0, None).unwrap();

        let dict = dw.field_buffer(0).unwrap().dict.as_ref().unwrap();
        assert_eq!(dict.len(), 2); // error, failed
        assert!(dict.find(b"error").is_some());
        assert!(dict.find(b"failed").is_some());
        assert!(dict.find(b"ERROR").is_none());
    }

    #[test]
    fn no_analyzer_keeps_original_bytes() {
        let mut schema = Schema::new();
        schema.add(FieldSpec::text("message"));
        let mut dw = DocWriter::new();
        let mut d0 = Document::new();
        d0.add("message", FieldValue::Text("ERROR Failed".to_string()));
        dw.add_document(&schema, d0, None).unwrap();

        let dict = dw.field_buffer(0).unwrap().dict.as_ref().unwrap();
        assert!(dict.find(b"ERROR").is_some());
        assert!(dict.find(b"error").is_none());
    }
```

（`field_buffer(field_number)`、`dict.find`/`len` 均为现有 API，见 line 703-713 的既有用法。）

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rustlucene-core doc_writer 2>&1 | tail -5`
Expected: FAIL（词典仍是原始字节，`dict.find(b"ERROR")` 命中 / analyzer 字段找不到小写 term）

- [ ] **Step 3: Write minimal implementation**

`crates/core/src/doc_writer.rs`：

1) `FieldBuf`（line 274-283）增加：

```rust
    /// Compiled analyzer chain for analyzer-configured text fields
    /// (spec §索引侧接入): compiled once per field per writer, reused
    /// across docs via its stateless template.
    pub analyzer: Option<crate::analysis::Analyzer>,
```

`FieldBuf::new`（line 286 起）构造：

```rust
        let analyzer = spec.analyzer.as_deref().map(|a| {
            crate::analysis::Analyzer::parse(a).expect("analyzer spec validated by Schema::add")
        });
```

2) 提取共享的 token 入库 helper（放在 `DocWriter` impl 外的自由函数）：

```rust
/// Indexes one analyzed token into the field dictionary; returns the RAM
/// delta. Shared by the legacy whitespace fast path and the analyzer path.
fn index_token(
    dict: &mut TermDict,
    doc_id: u32,
    tok: &[u8],
    has_positions: bool,
    position: u32,
) -> usize {
    let (id, is_new) = dict.lookup_or_insert_flag(tok);
    let new_doc = dict.recs[id as usize]
        .postings
        .add_occurrence(doc_id, if has_positions { Some(position) } else { None });
    (if is_new { TERM_RAM + tok.len() } else { 0 })
        + if new_doc {
            POSTING_NEW_DOC_RAM
        } else {
            POSTING_SAME_DOC_RAM
        }
}
```

3) 写入热路径（现 line 394-417 的循环）替换为：

```rust
                        let buf = self.buffers[number as usize].as_mut().unwrap();
                        let dict = buf.dict.as_mut().unwrap();
                        let has_positions = buf.spec.has_positions();
                        let mut saw_term = false;
                        let mut position = 0u32;
                        match &buf.analyzer {
                            Some(analyzer) => {
                                for tok in analyzer.analyze(&text) {
                                    self.ram_bytes +=
                                        index_token(dict, doc_id, &tok, has_positions, position);
                                    saw_term = true;
                                    position += 1;
                                }
                            }
                            None => {
                                for token in WhitespaceTokens::new(&text) {
                                    self.ram_bytes += index_token(
                                        dict,
                                        doc_id,
                                        token.as_bytes(),
                                        has_positions,
                                        position,
                                    );
                                    saw_term = true;
                                    position += 1;
                                }
                            }
                        }
                        if saw_term {
                            buf.doc_count += 1;
                        }
```

行为说明：无 analyzer 分支与原循环逐语句等价（同一 helper、同一 RAM 公式、同一 position 语义），字节级不变。

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p rustlucene-core`
Expected: PASS（新增 2 项 + 现有全绿）

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/doc_writer.rs
git commit -m "core: run configured analyzer chain at index time in DocWriter"
```

---

