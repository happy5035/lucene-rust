# Rust 搜索读路径 M1（地基 + Term 查询端到端）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为 rustlucene 增加搜索**读路径**的地基与 Term 查询端到端能力：Rust 直接读取本系统写出的 Lucene 9.12.3 兼容索引，执行 Term / MatchAll 查询（ConstantScore 语义），并用"Rust 搜索结果 vs Java Lucene 9.12.3 读取结果逐条 diff"的电池纳入 `make log-test` 作为决定性验收。对应已批准 spec `docs/superpowers/specs/2026-07-22-rust-search-design.md` 实施顺序的**阶段 1（地基）与阶段 2（Term 查询端到端）**。

**Architecture:** 方案 C（算法语义照抄 9.12.3、对象结构 Rust 化）。codec-lucene9 只懂字节：`IndexInput`（buffered 读抽象）→ 解码原语（for/pfor、packed）→ 提交点解析（segments_N/.si/.fnm）→ FST 读 → block-tree terms dict 读（.tip/.tim/.tmd）→ postings 枚举（.doc）。rustlucene-core 新增 `search/` 模块做搜索概念聚合：`SegmentReader`（段聚合）、`Reader`（多段）、`Query` enum（M1 只含 `Term` 与 `MatchAll`）、`trait DocIter`（DISI 语义）、collector（count / docID 序 topN / freqsum）与 `Searcher`。验证走既有 interop 设施：`rustlucene-cli searchdump` 输出与 Java `VerifySearchIndex` 输出逐条 diff，挂进 `interop/verify-log.sh`（`make log-test` 自动覆盖）。

**Tech Stack:** Rust（codec crate edition 2024、core crate edition 2021，`#![forbid(unsafe_code)]`，统一 `io::Result`）；既有依赖 crc32fast / rand（codec）、serde_json（core），不新增依赖；Java 9.12.3（`interop/java/lib/lucene-core-9.12.3.jar`）做 diff 基准；格式语义以 `reference/lucene-9.12.3/` 源码为准。

## Global Constraints

（摘自 spec，逐字或就近转述；所有 Task 共同遵守）

- **全部 ConstantScore 语义**：无评分 / norms / impact / Block-Max（spec §1）。collector 不排序打分，M1 只有 count 与 docID 序 topN。
- **前提假设**：读侧只保证读**本系统写出的**索引（无 delete、无 .liv、无 norms、无 vector 等）。读 Java 写的索引不在本期范围（spec §1）。segments_N 中 delCount/softDelCount 非零或 delGen/fieldInfosGen/docValuesGen 非 -1 一律报错拒绝。
- **格式对齐纪律**：执行语义逐行对照 9.12.3 源码，保持 docs 中 `File.java:line` 引用的项目惯例（spec §2 方案 C）。Java 源码在 `reference/lucene-9.12.3/lucene/core/src/java/org/apache/lucene/`（下文引用省略该前缀）。
- **`#![forbid(unsafe_code)]`**：两个 crate 均已声明，新增代码遵守。codec 读侧统一 `io::Result`；每个格式读入口校验 magic / codec header / version（对齐 `CodecUtil.checkHeader`），footer checksum 校验默认开启，损坏即 CorruptIndex 风格错误（`io::ErrorKind::InvalidData`，spec §5）。
- **标量先行**：SIMD 不在本计划（spec 阶段 9）。解码原语接口保持 `[u64; 128]` 整块形状（`for_util_decode` / `pfor_util_decode` 一次解一个 128 值块），为后续 SIMD bit-unpack 核留好块形状（spec §4a）。
- **mmap 不做**：先 buffered FileChannel 读（8KB 缓冲，对齐 `BufferedIndexInput`），留优化点（spec §1）。
- **明确不做（本计划范围外）**：positions/.pos、DV 读、BKD 读、stored 读、Boolean/Phrase/Prefix/Wildcard、排序 collector、JNI、skip-list 驱动的 `advance`（M1 `advance` = `next_doc` 循环；skip 框架字节必须**解析并跳过**以保证顺序迭代不错位，但不用于跳跃）、TermsEnum 顺序枚举（`next()`，阶段 7 再做）、块级批处理 collector（spec §4b，阶段 9）。
- **测试三层**：round-trip 单测（codec crate，写侧产出 → 读侧解码 → 断言重建，延续现有惯例）→ Rust 内部语义测试（core，固定语料断言 docID 序列）→ Java diff 终验（纳入 `make log-test`）。

## Pre-checks（已执行，基线绿）

```
$ cargo build
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.83s
$ cargo test -p codec-lucene9
test result: ok. 90 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

## 关键格式事实（本计划全部代码的字节级依据，已逐项对照 9.12.3 源码与写侧 Rust 源码核实）

1. **块与跳跃常量**：`BLOCK_SIZE = 128`（ForUtil.java:32，`postings_ll.rs:15`）；`MAX_EXCEPTIONS = 7`（PForUtil.java:30，`postings_ll.rs:17`）；`LEVEL1_NUM_DOCS = 4096`（Lucene912PostingsFormat.java:347-352，写侧 `postings.rs:36`）。
2. **写侧产出 skip 数据**（`postings.rs:517-667`）：每个满 128 块前有 level-0 skip 条目（`VLong skip0NumBytes` + `VInt15 docDelta` + `VLong15 blockTotalBytes` + impacts/pos 段）；每 4096 文档有一条 level-1 记录（`VInt docDelta` + hasFreqs 时 `VLong level1TotalBytes + Short numSkipBytes + Short impactBytes + scratch`），**写在本组 32 块之前**。顺序 `next_doc` 迭代必须 inline 消费这些字节（Java 侧证据：`Lucene912PostingsReader` `moveToNextLevel0Block` :573-587 与 `skipLevel1To` :522-546）。
3. **TermState（IntBlockTermState）**：`docStartFP / posStartFP / lastPosBlockOffset / singletonDocID`（Lucene912PostingsFormat.java:425-491）。.tim metadata blob 中编码（`postings.rs:72-100` 写，`Lucene912PostingsReader.decodeTerm` :235-277 读）：`VLong((fpDelta<<1))` 普通；`VLong((zigzag(idDelta)<<1)|1)` 连续 singleton；df==1 时 `VInt singletonDocID`；hasPositions 时 `VLong posFPDelta` 且 ttf>128 时 `VLong lastPosBlockOffset`。**df==1 singleton 不读 .doc，docID 直接取 singletonDocID**（`Lucene912PostingsReader.refillRemainder` :507）。
4. **FST 节点编码**：字节整体逆序写，节点地址 = 其最后一字节的偏移；arc 6 个 flag 位（FST.java:78-88）；写侧只产 unpacked 变长 arc 节点（`allowFixedLengthArcs == false`，`fst.rs:6-10`），读侧只需线性扫描分支（FST.findTargetArc :1100-1126）；`BIT_TARGET_NEXT` 的 target = 本节点全部 arc 读完后的当前位置（FST.readArc :966-990）；metadata = `CodecUtil` header("FST", 9) + emptyOutput flag（设则 `VInt len` + **整体逆序**的序列化 output）+ inputType 字节（BYTE1=0）+ `VLong startNode` + `VLong numBytes`（FST.readMetadata :455-500，写侧 `fst.rs:426-453`）。
5. **.tim block 布局**（SegmentTermsEnumFrame.loadBlock :145-240，写侧 `postings.rs:833-923`）：`VInt code`（`numEntries<<1 | isLastInFloor`）→ `VLong token`（`numSuffixBytes<<3 | isLeaf<<2 | compression`）→ suffix 字节 → suffixLengths（`VInt((n<<1)|allEqual)`，allEqual 时只存 1 字节）→ stats blob（`VInt len` + 字节）→ meta blob（`VInt len` + 字节）。非 leaf 块 entry 的 suffixLengths 首 VInt 低 1 位 = sub-block 标志，sub-block 紧跟 `VLong` 回退指针（nextNonLeaf :333-349）。stats 的 singleton run 编码 `(count-1)<<1|1`（decodeMetaData :433-481）。
6. **.tmd 字段记录**（Lucene90BlockTreeTermsReader 构造器 :180-242，写侧 `postings.rs:366-386`）：`VInt fieldNumber`、`VLong numTerms`、`readBytesRef rootCode`、`VLong sumTotalTermFreq`（无条件）、**仅非 DOCS 字段**再 `VLong sumDocFreq`（:192-198）、`VInt docCount`、minTerm/maxTerm、`VLong indexStartFP`、随后 FST metadata；文件尾 `readLong indexLength` + `readLong termsLength` + footer。postings header 也在 .tmd 内（第二个 index header + `VInt blockSize == 128`，Lucene912PostingsReader.init :188-206）。
7. **校验纪律**：segments_N/.si/.fnm/.tmd/.psm 用 checksum 流式读 + `CodecUtil.checkFooter`（CRC 覆盖从第 0 字节到 algorithmID 含，writeCRC :643-650）；.tim/.tip/.doc 正常打开只校验精确长度 + 尾部 16 字节结构（retrieveChecksum :623-647），不重算 CRC。

---

## Task 1: IndexInput + ChecksumIndexInput + FSDirectory::open_input

**Files:**
- Modify: `crates/codec-lucene9/src/io.rs`（追加读侧：`DataInput` trait、`IndexInput`、`ChecksumIndexInput`）
- Modify: `crates/codec-lucene9/src/directory.rs`（追加 `open_input` / `open_checksum_input`）
- Modify: `crates/codec-lucene9/src/codec_util.rs`（追加读侧校验助手；`FOOTER_ALGORITHM_ID` 改 `pub(crate)`）
- Modify: `crates/codec-lucene9/src/lib.rs`（re-export）
- Test: `crates/codec-lucene9/src/io.rs`、`crates/codec-lucene9/src/directory.rs`、`crates/codec-lucene9/src/codec_util.rs` 各自的 `#[cfg(test)]` 模块

**Interfaces:**
- Consumes: 现有 `IndexOutput` / `ChecksumIndexOutput` / `DataOutput`（io.rs）、`FSDirectory`（directory.rs）、`codec_util::{CODEC_MAGIC, FOOTER_MAGIC, FOOTER_LENGTH}`。
- Produces（后续 Task 依赖这些名字，不得改名）:
  ```rust
  // io.rs
  pub trait DataInput {
      fn read_byte(&mut self) -> io::Result<u8>;
      fn read_bytes(&mut self, buf: &mut [u8]) -> io::Result<()>;
      fn read_short(&mut self) -> io::Result<i16>;   // default, LE
      fn read_int(&mut self) -> io::Result<i32>;     // default, LE
      fn read_long(&mut self) -> io::Result<i64>;    // default, LE
      fn read_vint(&mut self) -> io::Result<i32>;    // default
      fn read_vlong(&mut self) -> io::Result<i64>;   // default
      fn read_zint(&mut self) -> io::Result<i32>;    // default, zigzag
      fn read_string(&mut self) -> io::Result<String>;                          // default
      fn read_map_of_strings(&mut self) -> io::Result<BTreeMap<String, String>>; // default
      fn read_set_of_strings(&mut self) -> io::Result<BTreeSet<String>>;         // default
      fn skip_bytes(&mut self, n: u64) -> io::Result<()>;                        // default
  }
  pub struct IndexInput { .. }
  impl IndexInput {
      pub fn from_file(file: File, length: u64) -> Self;
      pub fn in_memory(bytes: Vec<u8>) -> Self;
      pub fn length(&self) -> u64;
      pub fn file_pointer(&self) -> u64;
      pub fn seek(&mut self, pos: u64) -> io::Result<()>;
      pub fn slice(&self, offset: u64, length: u64) -> io::Result<IndexInput>;
  }
  pub struct ChecksumIndexInput { .. }
  impl ChecksumIndexInput {
      pub fn new(input: IndexInput) -> Self;
      pub fn get_checksum(&self) -> u64;
      pub fn file_pointer(&self) -> u64;
      pub fn length(&self) -> u64;
  }
  // directory.rs
  impl FSDirectory {
      pub fn open_input(&self, name: &str) -> io::Result<IndexInput>;
      pub fn open_checksum_input(&self, name: &str) -> io::Result<ChecksumIndexInput>;
  }
  // codec_util.rs
  pub(crate) fn corrupt(msg: impl Into<String>) -> io::Error;
  pub fn read_be_int(input: &mut impl DataInput) -> io::Result<u32>;
  pub fn read_be_long(input: &mut impl DataInput) -> io::Result<u64>;
  pub fn check_header(input: &mut impl DataInput, codec: &str, min_version: u32, max_version: u32) -> io::Result<u32>;
  pub fn check_index_header_id(input: &mut impl DataInput, expected_id: &[u8; 16]) -> io::Result<()>;
  pub fn check_index_header_suffix(input: &mut impl DataInput, expected: &str) -> io::Result<()>;
  pub fn check_index_header(input: &mut impl DataInput, codec: &str, min_version: u32, max_version: u32, expected_id: &[u8; 16], suffix: &str) -> io::Result<u32>;
  pub fn check_footer(input: &mut ChecksumIndexInput) -> io::Result<()>;
  pub fn check_footer_structure(input: &IndexInput, expected_length: u64) -> io::Result<()>;
  ```

### Steps

- [ ] **Step 1.1: 写失败测试** — 追加到 `crates/codec-lucene9/src/io.rs` 的 `mod tests`（模块内已有 `use super::*;` 与 `bytes(...)` 助手，直接复用）：

  ```rust
      // ---- read side (DataInput / IndexInput / ChecksumIndexInput) ----

      #[test]
      fn read_primitives_round_trip() {
          let bytes = bytes(|o| {
              o.write_byte(0xAB).unwrap();
              o.write_short(-2).unwrap();
              o.write_int(0x01020304).unwrap();
              o.write_long(-1).unwrap();
              o.write_vint(300).unwrap();
              o.write_vlong(1 << 35).unwrap();
              o.write_zint(-64).unwrap();
              o.write_string("héllo").unwrap();
              let mut m = BTreeMap::new();
              m.insert("k".to_string(), "v".to_string());
              o.write_map_of_strings(&m).unwrap();
              let mut s = BTreeSet::new();
              s.insert("a".to_string());
              s.insert("b".to_string());
              o.write_set_of_strings(&s).unwrap();
          });
          let mut i = IndexInput::in_memory(bytes);
          assert_eq!(i.read_byte().unwrap(), 0xAB);
          assert_eq!(i.read_short().unwrap(), -2);
          assert_eq!(i.read_int().unwrap(), 0x01020304);
          assert_eq!(i.read_long().unwrap(), -1);
          assert_eq!(i.read_vint().unwrap(), 300);
          assert_eq!(i.read_vlong().unwrap(), 1 << 35);
          assert_eq!(i.read_zint().unwrap(), -64);
          assert_eq!(i.read_string().unwrap(), "héllo");
          let m = i.read_map_of_strings().unwrap();
          assert_eq!(m.get("k").unwrap(), "v");
          let s = i.read_set_of_strings().unwrap();
          assert!(s.contains("a") && s.contains("b"));
          assert_eq!(i.file_pointer(), i.length());
      }

      #[test]
      fn read_beyond_end_is_eof() {
          let mut i = IndexInput::in_memory(vec![1]);
          assert_eq!(i.read_byte().unwrap(), 1);
          let err = i.read_byte().unwrap_err();
          assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
      }

      #[test]
      fn skip_bytes_moves_pointer() {
          let bytes = bytes(|o| o.write_bytes(&[7; 100]).unwrap());
          let mut i = IndexInput::in_memory(bytes);
          i.skip_bytes(90).unwrap();
          assert_eq!(i.file_pointer(), 90);
          assert_eq!(i.read_byte().unwrap(), 7);
          assert!(i.skip_bytes(11).is_err(), "skip past end must fail");
      }

      #[test]
      fn memory_slice_independent_positions() {
          let bytes = bytes(|o| o.write_bytes(&(0u8..100).collect::<Vec<_>>()).unwrap());
          let i = IndexInput::in_memory(bytes);
          let mut a = i.slice(10, 20).unwrap();
          let mut b = i.slice(50, 10).unwrap();
          assert_eq!(a.length(), 20);
          assert_eq!(a.read_byte().unwrap(), 10);
          assert_eq!(b.read_byte().unwrap(), 50);
          assert_eq!(a.read_byte().unwrap(), 11);
          assert!(i.slice(90, 20).is_err(), "slice past end must fail");
      }

      #[test]
      fn checksum_input_footer_round_trip() {
          let mut out = ChecksumIndexOutput::new(IndexOutput::in_memory());
          out.write_bytes(b"payload").unwrap();
          crate::codec_util::write_footer(&mut out).unwrap();
          let bytes = out.into_bytes();
          let mut input = ChecksumIndexInput::new(IndexInput::in_memory(bytes));
          let mut payload = [0u8; 7];
          input.read_bytes(&mut payload).unwrap();
          assert_eq!(&payload, b"payload");
          crate::codec_util::check_footer(&mut input).unwrap();
      }

      #[test]
      fn corrupted_payload_fails_footer() {
          let mut out = ChecksumIndexOutput::new(IndexOutput::in_memory());
          out.write_bytes(b"payload").unwrap();
          crate::codec_util::write_footer(&mut out).unwrap();
          let mut bytes = out.into_bytes();
          bytes[2] ^= 0xFF;
          let mut input = ChecksumIndexInput::new(IndexInput::in_memory(bytes));
          let mut payload = [0u8; 7];
          input.read_bytes(&mut payload).unwrap();
          assert!(crate::codec_util::check_footer(&mut input).is_err());
      }
  ```

  追加到 `crates/codec-lucene9/src/codec_util.rs` 的 `mod tests`：

  ```rust
      #[test]
      fn check_index_header_round_trip() {
          let id = [0x5Au8; 16];
          let mut out = ChecksumIndexOutput::new(IndexOutput::in_memory());
          write_index_header(&mut out, "Lucene90SegmentInfo", 0, &id, "").unwrap();
          let bytes = out.into_bytes();
          let mut input = ChecksumIndexInput::new(IndexInput::in_memory(bytes));
          let v = check_index_header(&mut input, "Lucene90SegmentInfo", 0, 0, &id, "").unwrap();
          assert_eq!(v, 0);
      }

      #[test]
      fn check_index_header_rejects_mismatch() {
          let id = [0x5Au8; 16];
          let mut out = ChecksumIndexOutput::new(IndexOutput::in_memory());
          write_index_header(&mut out, "segments", 10, &id, "2").unwrap();
          let bytes = out.into_bytes();
          // wrong codec name
          let mut i1 = ChecksumIndexInput::new(IndexInput::in_memory(bytes.clone()));
          assert!(check_index_header(&mut i1, "Segments", 10, 10, &id, "2").is_err());
          // wrong version range
          let mut i2 = ChecksumIndexInput::new(IndexInput::in_memory(bytes.clone()));
          assert!(check_index_header(&mut i2, "segments", 11, 12, &id, "2").is_err());
          // wrong id
          let mut i3 = ChecksumIndexInput::new(IndexInput::in_memory(bytes.clone()));
          assert!(check_index_header(&mut i3, "segments", 10, 10, &[0u8; 16], "2").is_err());
          // wrong suffix
          let mut i4 = ChecksumIndexInput::new(IndexInput::in_memory(bytes));
          assert!(check_index_header(&mut i4, "segments", 10, 10, &id, "3").is_err());
      }
  ```

  追加到 `crates/codec-lucene9/src/directory.rs` 的 `mod tests`：

  ```rust
      #[test]
      fn open_input_reads_what_output_wrote() {
          let root = temp_dir("open_input");
          let dir = FSDirectory::open(&root).unwrap();
          {
              let mut out = dir.create_output("f").unwrap();
              out.write_int(0x01020304).unwrap();
              out.write_vint(300).unwrap();
              out.flush().unwrap();
          }
          let mut input = dir.open_input("f").unwrap();
          assert_eq!(input.length(), 6);
          assert_eq!(input.read_int().unwrap(), 0x01020304);
          assert_eq!(input.read_vint().unwrap(), 300);
          // 文件读走 slice + 独立定位
          let mut s0 = input.slice(0, 4).unwrap();
          let mut s4 = input.slice(4, 1).unwrap();
          assert_eq!(s4.read_byte().unwrap(), 0xAC);
          assert_eq!(s0.read_int().unwrap(), 0x01020304);
          fs::remove_dir_all(&root).unwrap();
      }
  ```

- [ ] **Step 1.2: 跑测试确认失败**

  ```
  $ cargo test -p codec-lucene9 io:: 2>&1 | tail -5
  error[E0433]: failed to resolve: use of undeclared type `IndexInput`
  ```
  （测试引用的 `IndexInput` / `ChecksumIndexInput` / `check_index_header` / `check_footer` 尚不存在，编译失败即失败测试成立。）

- [ ] **Step 1.3: 最小实现** — `crates/codec-lucene9/src/io.rs` 追加（放在 `DataOutput` 定义之后、`mod tests` 之前；文件顶部 `use std::fs::File;` 已存在，新增 `use std::os::unix::fs::FileExt;`）：

  ```rust
  // ===========================================================================
  // Read side (DataInput / IndexInput / ChecksumIndexInput), mirroring
  // store/DataInput.java and store/BufferedIndexInput.java (9.12.3).
  // ===========================================================================

  /// Read buffer capacity (spec §3: buffered FileChannel 读, 8KB; cf.
  /// BufferedIndexInput.BUFFER_SIZE :32).
  const INPUT_BUFFER_CAPACITY: usize = 1 << 13;

  enum InputSource {
      /// Positional reads at `base + pos`; slices share the file handle via
      /// `try_clone` and shift `base` (std::os::unix::fs::FileExt::read_at).
      File { file: File, base: u64 },
      Memory(Vec<u8>),
  }

  /// Buffered, position-tracking data input with Lucene `DataInput` primitives
  /// (BufferedIndexInput). All multi-byte primitives are little-endian
  /// (DataInput.readInt/readLong, DataInput.java:94-100,183-185).
  pub struct IndexInput {
      source: InputSource,
      length: u64,
      buffer: [u8; INPUT_BUFFER_CAPACITY],
      buffer_start: u64, // absolute position of buffer[0]
      buffer_len: usize, // valid bytes in buffer
      position: u64,     // absolute position of the next byte to read
  }

  impl IndexInput {
      /// An input over `file[0..length]` (FSDirectory.openInput).
      pub fn from_file(file: File, length: u64) -> Self {
          IndexInput {
              source: InputSource::File { file, base: 0 },
              length,
              buffer: [0; INPUT_BUFFER_CAPACITY],
              buffer_start: 0,
              buffer_len: 0,
              position: 0,
          }
      }

      /// An input over an in-memory image (unit tests, in-memory blob parsing).
      pub fn in_memory(bytes: Vec<u8>) -> Self {
          IndexInput {
              length: bytes.len() as u64,
              source: InputSource::Memory(bytes),
              buffer: [0; INPUT_BUFFER_CAPACITY],
              buffer_start: 0,
              buffer_len: 0,
              position: 0,
          }
      }

      /// IndexInput.length (IndexInput.java:79).
      pub fn length(&self) -> u64 {
          self.length
      }

      /// BufferedIndexInput.getFilePointer (:371-374).
      pub fn file_pointer(&self) -> u64 {
          self.position
      }

      /// BufferedIndexInput.seek (:376-385).
      pub fn seek(&mut self, pos: u64) -> io::Result<()> {
          if pos > self.length {
              return Err(io::Error::new(
                  io::ErrorKind::UnexpectedEof,
                  format!("seek past EOF: {pos} > {}", self.length),
              ));
          }
          self.position = pos;
          Ok(())
      }

      /// IndexInput.slice (:121-122): an independent reader over
      /// `[offset, offset + length)` of this input.
      pub fn slice(&self, offset: u64, length: u64) -> io::Result<IndexInput> {
          if offset + length > self.length {
              return Err(io::Error::new(
                  io::ErrorKind::UnexpectedEof,
                  format!("slice [{offset}, +{length}) past EOF {}", self.length),
              ));
          }
          let source = match &self.source {
              InputSource::File { file, base } => InputSource::File {
                  file: file.try_clone()?,
                  base: base + offset,
              },
              InputSource::Memory(bytes) => InputSource::Memory(
                  bytes[offset as usize..(offset + length) as usize].to_vec(),
              ),
          };
          Ok(IndexInput {
              source,
              length,
              buffer: [0; INPUT_BUFFER_CAPACITY],
              buffer_start: 0,
              buffer_len: 0,
              position: 0,
          })
      }

      /// BufferedIndexInput.refill (:340-362).
      fn refill(&mut self) -> io::Result<()> {
          if self.position >= self.length {
              return Err(io::Error::new(
                  io::ErrorKind::UnexpectedEof,
                  format!("read past EOF: {}", self.length),
              ));
          }
          let n = INPUT_BUFFER_CAPACITY.min((self.length - self.position) as usize);
          match &self.source {
              InputSource::File { file, base } => {
                  file.read_at(&mut self.buffer[..n], base + self.position)?;
              }
              InputSource::Memory(bytes) => {
                  self.buffer[..n]
                      .copy_from_slice(&bytes[self.position as usize..self.position as usize + n]);
              }
          }
          self.buffer_start = self.position;
          self.buffer_len = n;
          Ok(())
      }

      fn buffered(&self) -> usize {
          (self.buffer_start + self.buffer_len as u64 - self.position) as usize
      }
  }

  /// DataInput-equivalent shared by raw and checksummed inputs (mirror of
  /// [`DataOutput`]), so decoders read through either without bypassing CRC.
  /// Composite readers have default implementations over `read_bytes`.
  pub trait DataInput {
      fn read_byte(&mut self) -> io::Result<u8>;
      fn read_bytes(&mut self, buf: &mut [u8]) -> io::Result<()>;

      /// Little-endian (DataInput.readShort, DataInput.java:82-86).
      fn read_short(&mut self) -> io::Result<i16> {
          let mut b = [0u8; 2];
          self.read_bytes(&mut b)?;
          Ok(i16::from_le_bytes(b))
      }

      /// Little-endian (DataInput.readInt, DataInput.java:94-100).
      fn read_int(&mut self) -> io::Result<i32> {
          let mut b = [0u8; 4];
          self.read_bytes(&mut b)?;
          Ok(i32::from_le_bytes(b))
      }

      /// Little-endian (DataInput.readLong, DataInput.java:183-185).
      fn read_long(&mut self) -> io::Result<i64> {
          let mut b = [0u8; 8];
          self.read_bytes(&mut b)?;
          Ok(i64::from_le_bytes(b))
      }

      /// 7 bits per group, low groups first; at most 5 bytes
      /// (DataInput.readVInt :136-165).
      fn read_vint(&mut self) -> io::Result<i32> {
          let mut v = 0u32;
          for i in 0..5 {
              let b = self.read_byte()?;
              v |= ((b & 0x7f) as u32) << (7 * i);
              if b & 0x80 == 0 {
                  return Ok(v as i32);
              }
          }
          Err(io::Error::new(
              io::ErrorKind::InvalidData,
              "vInt too long (DataInput.readVInt)",
          ))
      }

      /// 7 bits per group, low groups first; at most 9 bytes
      /// (DataInput.readVLong :235-286, negative values rejected like Java).
      fn read_vlong(&mut self) -> io::Result<i64> {
          let mut v = 0u64;
          for i in 0..9 {
              let b = self.read_byte()?;
              v |= ((b & 0x7f) as u64) << (7 * i);
              if b & 0x80 == 0 {
                  return Ok(v as i64);
              }
          }
          Err(io::Error::new(
              io::ErrorKind::InvalidData,
              "vLong too long (DataInput.readVLong)",
          ))
      }

      /// zigzag + VInt (DataInput.readZInt :173-175, BitUtil.zigZagDecode :299).
      /// 修正注记（2026-07-22 审查）：Java 用无符号移位 `i >>> 1`；先把 VInt
      /// 位模式按 u32 解码再移，否则 |n| >= 2^30（编码第 31 位置位）解错。
      fn read_zint(&mut self) -> io::Result<i32> {
          let v = self.read_vint()?;
          Ok(((v as u32 >> 1) as i32) ^ -(v & 1))
      }

      /// VInt **byte** length + UTF-8 (DataInput.readString :303-308).
      fn read_string(&mut self) -> io::Result<String> {
          let len = self.read_vint()? as usize;
          let mut bytes = vec![0u8; len];
          self.read_bytes(&mut bytes)?;
          String::from_utf8(bytes).map_err(|e| {
              io::Error::new(io::ErrorKind::InvalidData, format!("invalid UTF-8 string: {e}"))
          })
      }

      /// VInt size + (key, value) string pairs (DataInput.readMapOfStrings :334-349).
      fn read_map_of_strings(&mut self) -> io::Result<BTreeMap<String, String>> {
          let count = self.read_vint()? as usize;
          let mut map = BTreeMap::new();
          for _ in 0..count {
              let k = self.read_string()?;
              let v = self.read_string()?;
              map.insert(k, v);
          }
          Ok(map)
      }

      /// VInt size + strings (DataInput.readSetOfStrings :356-369).
      fn read_set_of_strings(&mut self) -> io::Result<BTreeSet<String>> {
          let count = self.read_vint()? as usize;
          let mut set = BTreeSet::new();
          for _ in 0..count {
              set.insert(self.read_string()?);
          }
          Ok(set)
      }

      /// IndexInput.skipBytes (:83-90). The default reads through (so the
      /// checksum wrapper stays correct); `IndexInput` overrides with a seek.
      fn skip_bytes(&mut self, mut n: u64) -> io::Result<()> {
          let mut scratch = [0u8; 4096];
          while n > 0 {
              let chunk = (n as usize).min(scratch.len());
              self.read_bytes(&mut scratch[..chunk])?;
              n -= chunk as u64;
          }
          Ok(())
      }
  }

  impl DataInput for IndexInput {
      /// BufferedIndexInput.readByte (:52-58).
      fn read_byte(&mut self) -> io::Result<u8> {
          if self.buffered() == 0 {
              self.refill()?;
          }
          let b = self.buffer[(self.position - self.buffer_start) as usize];
          self.position += 1;
          Ok(b)
      }

      /// BufferedIndexInput.readBytes (:91-133) — always through the buffer.
      fn read_bytes(&mut self, mut buf: &mut [u8]) -> io::Result<()> {
          while !buf.is_empty() {
              if self.buffered() == 0 {
                  self.refill()?;
              }
              let n = self.buffered().min(buf.len());
              let start = (self.position - self.buffer_start) as usize;
              buf[..n].copy_from_slice(&self.buffer[start..start + n]);
              self.position += n as u64;
              let rest = std::mem::take(&mut buf);
              buf = &mut rest[n..];
          }
          Ok(())
      }

      /// IndexInput.skipBytes (:83-90) = seek(getFilePointer() + numBytes).
      fn skip_bytes(&mut self, n: u64) -> io::Result<()> {
          self.seek(self.position + n)
      }
  }

  /// Wraps an `IndexInput` with a running CRC32 over every byte read
  /// (store/ChecksumIndexInput; CRC32 algorithm = java.util.zip.CRC32,
  /// BufferedChecksumIndexInput.java:20,34). Sequential reads only — segments_N,
  /// .si, .fnm, .tmd, .psm are parsed straight through.
  pub struct ChecksumIndexInput {
      input: IndexInput,
      digest: crc32fast::Hasher,
  }

  impl ChecksumIndexInput {
      pub fn new(input: IndexInput) -> Self {
          ChecksumIndexInput {
              input,
              digest: crc32fast::Hasher::new(),
          }
      }

      /// CRC32 of everything read so far (CodecUtil.writeCRC :643-650 takes this
      /// value *after* the footer magic + algorithmID have been read).
      pub fn get_checksum(&self) -> u64 {
          self.digest.clone().finalize() as u64
      }

      pub fn file_pointer(&self) -> u64 {
          self.input.file_pointer()
      }

      pub fn length(&self) -> u64 {
          self.input.length()
      }
  }

  impl DataInput for ChecksumIndexInput {
      fn read_byte(&mut self) -> io::Result<u8> {
          let b = self.input.read_byte()?;
          self.digest.update(&[b]);
          Ok(b)
      }

      fn read_bytes(&mut self, buf: &mut [u8]) -> io::Result<()> {
          self.input.read_bytes(buf)?;
          self.digest.update(buf);
          Ok(())
      }
  }
  ```

  `crates/codec-lucene9/src/codec_util.rs`：`const FOOTER_ALGORITHM_ID` 改 `pub(crate)`，并在 `write_footer` 之后追加：

  ```rust
  /// CorruptIndexException equivalent (spec §5: 损坏即 CorruptIndex 错误).
  pub(crate) fn corrupt(msg: impl Into<String>) -> io::Error {
      io::Error::new(io::ErrorKind::InvalidData, format!("corrupt index: {}", msg.into()))
  }

  use crate::io::{ChecksumIndexInput, DataInput, IndexInput};

  /// Big-endian int (CodecUtil.readBEInt :667-672). Header/footer ints are BE
  /// while file bodies are LE — do not mix up.
  pub fn read_be_int(input: &mut impl DataInput) -> io::Result<u32> {
      let mut b = [0u8; 4];
      input.read_bytes(&mut b)?;
      Ok(u32::from_be_bytes(b))
  }

  /// Big-endian long (CodecUtil.readBELong :675-677).
  pub fn read_be_long(input: &mut impl DataInput) -> io::Result<u64> {
      let mut b = [0u8; 8];
      input.read_bytes(&mut b)?;
      Ok(u64::from_be_bytes(b))
  }

  /// CodecUtil.checkHeader (:182-195) + checkHeaderNoMagic (:201-218).
  /// Returns the version found.
  pub fn check_header(
      input: &mut impl DataInput,
      codec: &str,
      min_version: u32,
      max_version: u32,
  ) -> io::Result<u32> {
      let magic = read_be_int(input)?;
      if magic != CODEC_MAGIC {
          return Err(corrupt(format!("bad codec magic {magic:#x}")));
      }
      let name = input.read_string()?;
      if name != codec {
          return Err(corrupt(format!("expected codec {codec}, found {name}")));
      }
      let version = read_be_int(input)?;
      if !(min_version..=max_version).contains(&version) {
          return Err(corrupt(format!(
              "codec {codec} version {version} outside [{min_version}, {max_version}]"
          )));
      }
      Ok(version)
  }

  /// CodecUtil.checkIndexHeaderID (:363-375).
  pub fn check_index_header_id(
      input: &mut impl DataInput,
      expected_id: &[u8; 16],
  ) -> io::Result<()> {
      let mut id = [0u8; 16];
      input.read_bytes(&mut id)?;
      if &id != expected_id {
          return Err(corrupt("index header ID mismatch"));
      }
      Ok(())
  }

  /// CodecUtil.checkIndexHeaderSuffix (:378-389).
  pub fn check_index_header_suffix(
      input: &mut impl DataInput,
      expected: &str,
  ) -> io::Result<()> {
      let len = input.read_byte()? as usize;
      let mut bytes = vec![0u8; len];
      input.read_bytes(&mut bytes)?;
      if bytes != expected.as_bytes() {
          return Err(corrupt(format!(
              "index header suffix mismatch: expected {expected:?}"
          )));
      }
      Ok(())
  }

  /// CodecUtil.checkIndexHeader (:246-258) = checkHeader + ID + suffix.
  pub fn check_index_header(
      input: &mut impl DataInput,
      codec: &str,
      min_version: u32,
      max_version: u32,
      expected_id: &[u8; 16],
      suffix: &str,
  ) -> io::Result<u32> {
      let version = check_header(input, codec, min_version, max_version)?;
      check_index_header_id(input, expected_id)?;
      check_index_header_suffix(input, suffix)?;
      Ok(version)
  }

  /// CodecUtil.checkFooter (:432-445) + validateFooter (:560-598): the stream
  /// must have exactly `FOOTER_LENGTH` bytes left; the CRC covers every byte
  /// through algorithmID inclusive (writeCRC :643-650).
  pub fn check_footer(input: &mut ChecksumIndexInput) -> io::Result<()> {
      let remaining = input.length() - input.file_pointer();
      if remaining != FOOTER_LENGTH as u64 {
          return Err(corrupt(format!(
              "expected {FOOTER_LENGTH} footer bytes, {remaining} remaining"
          )));
      }
      let magic = read_be_int(input)?;
      if magic != FOOTER_MAGIC {
          return Err(corrupt(format!("bad footer magic {magic:#x}")));
      }
      let algorithm_id = read_be_int(input)?;
      if algorithm_id != FOOTER_ALGORITHM_ID {
          return Err(corrupt(format!("bad footer algorithmID {algorithm_id}")));
      }
      let expected = input.get_checksum();
      let actual = read_be_long(input)?;
      if actual != expected {
          return Err(corrupt(format!(
              "footer CRC mismatch: expected {expected:#x}, found {actual:#x}"
          )));
      }
      Ok(())
  }

  /// CodecUtil.retrieveChecksum (:623-647) as used on the normal open path of
  /// the big data files (.tim/.tip/.doc): exact length + trailing footer
  /// structure, without recomputing the CRC (Lucene90BlockTreeTermsReader
  /// .java:328-336, Lucene912PostingsReader.java:150-172).
  pub fn check_footer_structure(input: &IndexInput, expected_length: u64) -> io::Result<()> {
      if input.length() != expected_length {
          return Err(corrupt(format!(
              "length mismatch: expected {expected_length}, found {}",
              input.length()
          )));
      }
      if expected_length < FOOTER_LENGTH as u64 {
          return Err(corrupt("file shorter than footer"));
      }
      let mut tail = input.slice(expected_length - FOOTER_LENGTH as u64, FOOTER_LENGTH as u64)?;
      if read_be_int(&mut tail)? != FOOTER_MAGIC {
          return Err(corrupt("bad footer magic"));
      }
      if read_be_int(&mut tail)? != FOOTER_ALGORITHM_ID {
          return Err(corrupt("bad footer algorithmID"));
      }
      Ok(())
  }
  ```

  `crates/codec-lucene9/src/directory.rs`：顶部 `use crate::io::{ChecksumIndexInput, ChecksumIndexOutput, IndexInput, IndexOutput};`（替换现有 `use crate::io::{ChecksumIndexOutput, IndexOutput};`），`impl FSDirectory` 内 `create_output` 之后追加：

  ```rust
      /// Directory.openInput: opens an existing file for reading.
      pub fn open_input(&self, name: &str) -> io::Result<IndexInput> {
          let file = File::open(self.resolve(name))?;
          let length = file.metadata()?.len();
          Ok(IndexInput::from_file(file, length))
      }

      /// Directory.openChecksumInput (commit/codec metadata files are read
      /// this way, with CodecUtil.checkFooter at the end).
      pub fn open_checksum_input(&self, name: &str) -> io::Result<ChecksumIndexInput> {
          Ok(ChecksumIndexInput::new(self.open_input(name)?))
      }
  ```

  `crates/codec-lucene9/src/lib.rs`：`pub use io::{ChecksumIndexOutput, IndexOutput};` 改为：

  ```rust
  pub use io::{ChecksumIndexInput, ChecksumIndexOutput, DataInput, IndexInput, IndexOutput};
  ```

- [ ] **Step 1.4: 跑测试确认通过**

  ```
  $ cargo test -p codec-lucene9 2>&1 | tail -3
  test result: ok. 99 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  ```
  （90 基线 + 本任务 9 个新测试全绿；数量以实际为准，必须 0 failed。）

- [ ] **Step 1.5: 提交**

  ```
  git add crates/codec-lucene9/src/io.rs crates/codec-lucene9/src/directory.rs crates/codec-lucene9/src/codec_util.rs crates/codec-lucene9/src/lib.rs
  git commit -m "feat: IndexInput/ChecksumIndexInput read path + CodecUtil check helpers"
  ```

---

## Task 2: 解码原语 for/pfor/group-vint/vint15 decode（postings_ll.rs 追加）

**Files:**
- Modify: `crates/codec-lucene9/src/postings_ll.rs`（追加读侧解码函数 + 测试）
- Test: `crates/codec-lucene9/src/postings_ll.rs` 的 `#[cfg(test)]` 模块

**Interfaces:**
- Consumes: T1 的 `crate::io::DataInput`；本文件写侧 `for_util_encode` / `for_delta_util_encode` / `pfor_util_encode` / `write_group_vints` / `write_vint15` / `write_vlong15` / `write_msb_vlong`（round-trip 对拍）；私有助手 `primitive_mask`。
- Produces:
  ```rust
  pub fn read_group_vints(input: &mut impl DataInput, values: &mut [u32]) -> io::Result<()>;
  pub fn for_util_decode(input: &mut impl DataInput, values: &mut [u64; BLOCK_SIZE], bpv: u8) -> io::Result<()>;
  pub fn for_delta_util_decode(input: &mut impl DataInput, deltas: &mut [u64; BLOCK_SIZE]) -> io::Result<()>;
  pub fn pfor_util_decode(input: &mut impl DataInput, values: &mut [u64; BLOCK_SIZE]) -> io::Result<()>;
  pub fn read_vint15(input: &mut impl DataInput) -> io::Result<u32>;
  pub fn read_vlong15(input: &mut impl DataInput) -> io::Result<u64>;
  pub fn read_msb_vlong(input: &mut impl DataInput) -> io::Result<u64>;
  ```
  接口刻意保持 `[u64; BLOCK_SIZE]` 整块形状，为后续 SIMD bit-unpack 核留位（spec §4a）。

### Steps

- [ ] **Step 2.1: 写失败测试** — 追加到 `postings_ll.rs` 的 `mod tests`（复用模块内已有 `enc` / `unhex` / `java_vector` 助手）：

  ```rust
      fn dec<T>(bytes: &[u8], f: impl FnOnce(&mut IndexInput) -> io::Result<T>) -> T {
          f(&mut IndexInput::in_memory(bytes.to_vec())).unwrap()
      }

      #[test]
      fn group_vints_decode_round_trip() {
          // full group + tail (len 6) and the exact layout vector of the write side
          let values = [0x12, 0x3456, 0x789ABC, 0xF0DE1A2B, 1, 300];
          let bytes = enc(|o| write_group_vints(o, &values));
          let mut back = [0u32; 6];
          dec(&bytes, |i| read_group_vints(i, &mut back));
          assert_eq!(back, values);
      }

      #[test]
      fn for_util_decode_round_trip_all_bpv() {
          for bpv in [1u8, 2, 3, 4, 5, 7, 8, 9, 11, 12, 16, 17, 24, 25, 31, 32] {
              let mask = if bpv == 64 { u64::MAX } else { (1u64 << bpv) - 1 };
              let v = java_vector(|x, i| *x = (i as u64 * 37 + 5) & mask);
              let bytes = enc(|o| for_util_encode(o, &v, bpv));
              let mut back = [0u64; BLOCK_SIZE];
              dec(&bytes, |i| for_util_decode(i, &mut back, bpv));
              assert_eq!(back, v, "bpv {bpv}");
          }
      }

      #[test]
      fn for_util_decode_matches_java_vectors() {
          // decode the reference vectors dumped from real Lucene (see encode tests)
          let v9 = java_vector(|x, i| *x = (i as u64 * 37) % 512);
          let bytes = unhex("1ef076a04e502600c902dfb2d562cb127b1578c57e757b258827c8d78987c937743a24ea549a044ae14ccdfcf9ace55c695f640f6fbf6a6fce71cd21ccd1ce815884383418e47894df96b7468ff6e7a63aa9305926093cb997bb946b921b97cb0fce4f7e0e2e4edef6e0a690d64086f002f32ea31a53060391059cb59765921558185bc85a785828db2abbda9b8afb3a");
          let mut back = [0u64; BLOCK_SIZE];
          dec(&bytes, |i| for_util_decode(i, &mut back, 9));
          assert_eq!(back, v9);
          let v16 = java_vector(|x, i| *x = (i as u64 * 37) % 65536);
          let bytes = unhex("e00d4009a0040000050e6509c50425002a0e8a09ea044a004f0eaf090f056f00740ed40934059400990ef9095905b900be0e1e0a7e05de00e30e430aa3050301080f680ac80528012d0f8d0aed054d01520fb20a12067201770fd70a370697019c0ffc0a5c06bc01c10f210b8106e101e60f460ba60606020b106b0bcb062b023010900bf00650025510b50b150775027a10da0b3a079a029f10ff0b5f07bf02c410240c8407e402e910490ca90709030e116e0cce072e033311930cf30753035811b80c180878037d11dd0c3d089d03a211020d6208c203c711270d8708e703ec114c0dac080c041112710dd10831043612960df60856045b12bb0d1b097b04");
          let mut back = [0u64; BLOCK_SIZE];
          dec(&bytes, |i| for_util_decode(i, &mut back, 16));
          assert_eq!(back, v16);
      }

      #[test]
      fn for_delta_decode_all_ones_and_mixed() {
          let mut back = [0u64; BLOCK_SIZE];
          // all-ones collapses to a single 0 byte
          dec(&[0x00], |i| for_delta_util_decode(i, &mut back));
          assert_eq!(back, [1u64; BLOCK_SIZE]);
          // mixed deltas (Java reference vector from the encode test)
          let mixed = java_vector(|x, i| *x = (i as u64 * 37) % 50 + 1);
          let bytes = unhex("06fa0ed14cd58dd90625a22e1b0c590d9a5c6d5cae3a243a65833887798bba6330dc06bd44be85b7c6029a0213e250e291396531a6151c195d6d30667164b24528bac69a3c987d98bee391e70acb48c289175d159e1617ff544228406940aa2020");
          dec(&bytes, |i| for_delta_util_decode(i, &mut back));
          assert_eq!(back, mixed);
      }

      #[test]
      fn pfor_decode_matches_java_vectors() {
          let mut back = [0u64; BLOCK_SIZE];
          // plain
          let freqs = java_vector(|x, i| *x = (i % 5 + 1) as u64);
          let bytes = unhex("03724e29a594724e2996714f28a696714fa595704f2aa5957029a496724d29a4964c2aa695734c2aa6734e29a594734e29");
          dec(&bytes, |i| pfor_util_decode(i, &mut back));
          assert_eq!(back, freqs);
          // exceptions at 3, 77, 100
          let mut v = freqs;
          v[3] = 3000;
          v[77] = 65535;
          v[100] = 999;
          let bytes = unhex("6803020105040302010403020105040302050403020105040301050403020105b802e705040302010503020105040302010403020105040302050403020105040301050403020105040201050403020105030201050403020105040201050403020104030201050403020504030201050403010504ff0201050402010504030201050302010504030201030b4dff6403");
          dec(&bytes, |i| pfor_util_decode(i, &mut back));
          assert_eq!(back, v);
          // constant branch with one exception
          let mut values = [1u64; BLOCK_SIZE];
          values[3] = 255;
          let bytes = enc(|o| pfor_util_encode(o, &values));
          dec(&bytes, |i| pfor_util_decode(i, &mut back));
          assert_eq!(back, values);
      }

      #[test]
      fn vint15_vlong15_round_trip() {
          for v in [0u32, 1, 0x7FFF, 0x8000, 0x1_FFFF, u32::MAX] {
              let bytes = enc(|o| write_vint15(o, v));
              assert_eq!(dec(&bytes, read_vint15), v, "vint15 {v}");
          }
          for v in [0u64, 0x7FFF, 0x8000, 1 << 40] {
              let bytes = enc(|o| write_vlong15(o, v));
              assert_eq!(dec(&bytes, read_vlong15), v, "vlong15 {v}");
          }
      }

      #[test]
      fn msb_vlong_round_trip() {
          for v in [0u64, 1, 0x80, 0x7FFF, 1 << 35, u64::MAX >> 1] {
              let bytes = enc(|o| write_msb_vlong(o, v));
              assert_eq!(dec(&bytes, read_msb_vlong), v, "msb_vlong {v}");
          }
      }
  ```

  注：测试模块还需引入 `IndexInput` —— 在 `mod tests` 顶部（`use super::*;` 之后）追加 `use crate::io::IndexInput;`（`super::*` 只带入实现代码已导入的 `DataInput`）。

- [ ] **Step 2.2: 跑测试确认失败**

  ```
  $ cargo test -p codec-lucene9 postings_ll:: 2>&1 | tail -5
  error[E0425]: cannot find function `read_group_vints` in this scope
  ```

- [ ] **Step 2.3: 最小实现** — `crates/codec-lucene9/src/postings_ll.rs`：顶部 `use crate::io::DataOutput;` 改为 `use crate::io::{DataInput, DataOutput};`，并在 `write_msb_vlong` 之后、`#[cfg(test)]` 之前追加：

  ```rust
  // ---------------------------------------------------------------------------
  // Read side — exact mirrors of the encoders above.
  // ---------------------------------------------------------------------------

  /// GroupVIntUtil.readGroupVInt (:42-54) + DataInput.readGroupVInts (:110-118):
  /// full groups of 4 via the flag byte (2 bits per value = byte count - 1,
  /// most significant bits first), tail as plain VInts. Mirrors
  /// [`write_group_vints`].
  pub fn read_group_vints(input: &mut impl DataInput, values: &mut [u32]) -> io::Result<()> {
      let full_groups = values.len() / 4;
      for g in 0..full_groups {
          let flag = input.read_byte()?;
          for (i, v) in values[g * 4..g * 4 + 4].iter_mut().enumerate() {
              let size = ((flag >> (6 - 2 * i)) & 0x3) + 1;
              let mut value = 0u32;
              for j in 0..size {
                  value |= (input.read_byte()? as u32) << (8 * j);
              }
              *v = value;
          }
      }
      for v in &mut values[full_groups * 4..] {
          *v = input.read_vint()? as u32;
      }
      Ok(())
  }

  /// ForUtil.expand8 (:59-71): inverse of [`collapse8`].
  fn expand8(collapsed: &[u64; BLOCK_SIZE], values: &mut [u64; BLOCK_SIZE]) {
      for i in 0..16 {
          let l = collapsed[i];
          for j in 0..8 {
              values[16 * j + i] = (l >> (56 - 8 * j)) & 0xFF;
          }
      }
  }

  /// ForUtil.expand16 (:87-95): inverse of [`collapse16`].
  fn expand16(collapsed: &[u64; BLOCK_SIZE], values: &mut [u64; BLOCK_SIZE]) {
      for i in 0..32 {
          let l = collapsed[i];
          for j in 0..4 {
              values[32 * j + i] = (l >> (48 - 16 * j)) & 0xFFFF;
          }
      }
  }

  /// ForUtil.expand32 (:103-109): inverse of [`collapse32`].
  fn expand32(collapsed: &[u64; BLOCK_SIZE], values: &mut [u64; BLOCK_SIZE]) {
      for i in 0..64 {
          values[i] = collapsed[i] >> 32;
          values[64 + i] = collapsed[i] & 0xFFFF_FFFF;
      }
  }

  /// ForUtil.decode (:291-394) via a single generic routine covering every
  /// (bpv, primitive) pair — the exact inverse of [`for_util_encode_primitive`]
  /// (Java's per-bpv specializations are unrolled forms of the same layout;
  /// decodeSlow :199-223 proves a generic decoder exists).
  fn for_util_decode_primitive(
      input: &mut impl DataInput,
      values: &mut [u64; BLOCK_SIZE],
      bpv: u32,
      primitive: u32,
  ) -> io::Result<()> {
      debug_assert!(bpv >= 1 && bpv <= 32);
      let num_longs = BLOCK_SIZE * primitive as usize / 64;
      let num_longs_per_shift = (bpv * 2) as usize;
      let mut tmp = [0u64; BLOCK_SIZE / 2];
      for lane in tmp.iter_mut().take(num_longs_per_shift) {
          *lane = input.read_long()? as u64;
      }

      let mut longs = [0u64; BLOCK_SIZE];
      let mut idx = 0usize;
      let value_mask = primitive_mask(primitive, bpv);
      // Whole bpv planes (inverse of the encode shift loops, ForUtil.java:142-149).
      let mut shift = primitive as i64 - bpv as i64;
      loop {
          for i in 0..num_longs_per_shift {
              longs[idx] = (tmp[i] >> shift) & value_mask;
              idx += 1;
          }
          shift -= bpv as i64;
          if shift < 0 {
              break;
          }
      }
      // Remainder planes, split across tmp slots (inverse of ForUtil.java:151-187).
      let remaining_bits_per_long = (shift + bpv as i64) as u32;
      if idx < num_longs {
          let mask_remaining = primitive_mask(primitive, remaining_bits_per_long);
          let mut tmp_idx = 0usize;
          let mut remaining_bits_per_value = bpv;
          while idx < num_longs {
              if remaining_bits_per_value >= remaining_bits_per_long {
                  remaining_bits_per_value -= remaining_bits_per_long;
                  longs[idx] |= (tmp[tmp_idx] & mask_remaining) << remaining_bits_per_value;
                  tmp_idx += 1;
                  if remaining_bits_per_value == 0 {
                      idx += 1;
                      remaining_bits_per_value = bpv;
                  }
              } else {
                  let mask1 = primitive_mask(primitive, remaining_bits_per_value);
                  let mask2 =
                      primitive_mask(primitive, remaining_bits_per_long - remaining_bits_per_value);
                  longs[idx] |=
                      (tmp[tmp_idx] >> (remaining_bits_per_long - remaining_bits_per_value)) & mask1;
                  idx += 1;
                  remaining_bits_per_value =
                      bpv - remaining_bits_per_long + remaining_bits_per_value;
                  longs[idx] |= (tmp[tmp_idx] & mask2) << remaining_bits_per_value;
                  tmp_idx += 1;
              }
          }
      }

      match primitive {
          8 => expand8(&longs, values),
          16 => expand16(&longs, values),
          32 => expand32(&longs, values),
          _ => unreachable!(),
      }
      Ok(())
  }

  /// ForUtil.decode public entry (:291-394). `bpv == 0` encodes nothing
  /// (mirrors [`for_util_encode`]); primitive thresholds :118-133.
  pub fn for_util_decode(
      input: &mut impl DataInput,
      values: &mut [u64; BLOCK_SIZE],
      bpv: u8,
  ) -> io::Result<()> {
      if bpv == 0 {
          values.fill(0);
          return Ok(());
      }
      if bpv > 32 {
          return Err(io::Error::new(
              io::ErrorKind::InvalidData,
              format!("ForUtil packs at most 32 bits per value, got {bpv}"),
          ));
      }
      let primitive = if bpv <= 8 { 8 } else if bpv <= 16 { 16 } else { 32 };
      for_util_decode_primitive(input, values, bpv as u32, primitive)
  }

  /// ForDeltaUtil.decodeDeltas (:276-283 without the prefix-sum step): a 0
  /// byte means all-ones (dense postings); otherwise 1 byte bpv followed by
  /// the ForUtil bit stream. Note the primitive thresholds (:261-269) differ
  /// from ForUtil's. The caller applies the prefix sum
  /// (Lucene912PostingsReader.prefixSum :208-213).
  pub fn for_delta_util_decode(
      input: &mut impl DataInput,
      deltas: &mut [u64; BLOCK_SIZE],
  ) -> io::Result<()> {
      let bpv = input.read_byte()?;
      if bpv == 0 {
          deltas.fill(1);
          return Ok(());
      }
      if bpv > 32 {
          return Err(io::Error::new(
              io::ErrorKind::InvalidData,
              format!("ForDeltaUtil packs at most 32 bits per value, got {bpv}"),
          ));
      }
      let primitive = if bpv <= 4 { 8 } else if bpv <= 11 { 16 } else { 32 };
      for_util_decode_primitive(input, deltas, bpv as u32, primitive)
  }

  /// PForUtil.decode (:117-130): token byte = numExceptions<<5 | bitsPerValue;
  /// bpv 0 = constant block (one VLong); then (position, high-byte) exception
  /// pairs appended after the packed data. Mirrors [`pfor_util_encode`].
  pub fn pfor_util_decode(
      input: &mut impl DataInput,
      values: &mut [u64; BLOCK_SIZE],
  ) -> io::Result<()> {
      let token = input.read_byte()?;
      let bits_per_value = token & 0x1f;
      let num_exceptions = token >> 5;
      if bits_per_value == 0 {
          let v = input.read_vlong()? as u64;
          values.fill(v);
      } else {
          for_util_decode(input, values, bits_per_value)?;
      }
      for _ in 0..num_exceptions {
          let pos = input.read_byte()? as usize;
          let high = input.read_byte()? as u64;
          values[pos] |= high << bits_per_value;
      }
      Ok(())
  }

  /// Lucene912PostingsReader.readVInt15 (:2043-2047): LE short; when the top
  /// bit is set, a VInt carries the high bits. Mirrors [`write_vint15`].
  pub fn read_vint15(input: &mut impl DataInput) -> io::Result<u32> {
      let s = input.read_short()?;
      if s >= 0 {
          Ok(s as u32)
      } else {
          Ok(((s as u16 & 0x7FFF) as u32) | ((input.read_vint()? as u32) << 15))
      }
  }

  /// Lucene912PostingsReader.readVLong15 (:2055-2062). Mirrors [`write_vlong15`].
  pub fn read_vlong15(input: &mut impl DataInput) -> io::Result<u64> {
      let s = input.read_short()?;
      if s >= 0 {
          Ok(s as u64)
      } else {
          Ok(((s as u16 & 0x7FFF) as u64) | ((input.read_vlong()? as u64) << 15))
      }
  }

  /// FieldReader.readMSBVLong (FieldReader.java:126-136): 7-bit groups in
  /// MSB-first order, continuation bit on all but the last byte. Mirrors
  /// [`write_msb_vlong`].
  pub fn read_msb_vlong(input: &mut impl DataInput) -> io::Result<u64> {
      let mut l = 0u64;
      loop {
          let b = input.read_byte()?;
          l = (l << 7) | ((b & 0x7f) as u64);
          if b & 0x80 == 0 {
              return Ok(l);
          }
      }
  }
  ```

- [ ] **Step 2.4: 跑测试确认通过**

  ```
  $ cargo test -p codec-lucene9 postings_ll:: 2>&1 | tail -3
  test result: ok. 20 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  ```

- [ ] **Step 2.5: 提交**

  ```
  git add crates/codec-lucene9/src/postings_ll.rs
  git commit -m "feat: postings decode primitives (for/pfor/group-vint/vint15/msb-vlong)"
  ```

---

## Task 3: DirectReader / DirectMonotonicReader（packed.rs 追加）

M1 的 Term 路径不消费这两个 reader（它们服务 stored fields .fdx 与后续 DV 读），但属于 spec 阶段 1 地基，与写侧 `direct_writer_encode` / `direct_monotonic_write` 配对交付并 round-trip 锁定。

**Files:**
- Modify: `crates/codec-lucene9/src/packed.rs`（追加读侧 struct + 测试）
- Test: `crates/codec-lucene9/src/packed.rs` 的 `#[cfg(test)]` 模块（现有测试内的 `MonotonicReader` 测试替身改为走生产代码）

**Interfaces:**
- Consumes: 写侧 `direct_writer_encode` / `direct_monotonic_write` / `direct_writer_unsigned_bits_required`；`SUPPORTED_BITS_PER_VALUE`。
- Produces:
  ```rust
  pub struct DirectReader<'a> { .. }
  impl<'a> DirectReader<'a> {
      pub fn new(data: &'a [u8], bits_per_value: u32, offset: u64) -> io::Result<Self>;
      pub fn get(&self, index: u64) -> u64;
  }
  pub struct DirectMonotonicReader<'a> { .. }
  impl<'a> DirectMonotonicReader<'a> {
      pub const META_RECORD_BYTES: usize = 21;
      pub fn new(meta: &[u8], data: &'a [u8], num_values: usize, block_shift: u32) -> io::Result<Self>;
      pub fn get(&self, index: u64) -> u64;
  }
  ```

### Steps

- [ ] **Step 3.1: 写失败测试** — `packed.rs` 的 `mod tests` 中**删除**测试替身 `struct MonotonicReader` 及其 `impl`，并在 `round_trip` 助手中改用生产代码（`round_trip` 函数体替换为）：

  ```rust
      fn round_trip(values: &[u64], block_shift: u32) {
          let mut meta = ChecksumIndexOutput::new(IndexOutput::in_memory());
          let mut data = ChecksumIndexOutput::new(IndexOutput::in_memory());
          direct_monotonic_write(&mut meta, &mut data, values, block_shift).unwrap();
          let meta_bytes = meta.into_bytes();
          let data_bytes = data.into_bytes();
          let reader =
              DirectMonotonicReader::new(&meta_bytes, &data_bytes, values.len(), block_shift)
                  .unwrap();
          for (i, &v) in values.iter().enumerate() {
              assert_eq!(reader.get(i as u64), v, "value {i} of {values:?}");
          }
      }
  ```

  并追加新测试：

  ```rust
      #[test]
      fn direct_reader_round_trip_all_bpv() {
          let mut state = 0x243F6A8885A308D3u64;
          let mut rand = move || {
              state ^= state << 13;
              state ^= state >> 7;
              state ^= state << 17;
              state
          };
          for bpv in SUPPORTED_BITS_PER_VALUE {
              let mask = if bpv == 64 { u64::MAX } else { (1u64 << bpv) - 1 };
              let values: Vec<u64> = (0..100).map(|_| rand() & mask).collect();
              let bytes = direct_writer_encode(&values, bpv);
              let reader = DirectReader::new(&bytes, bpv, 0).unwrap();
              for (i, &v) in values.iter().enumerate() {
                  assert_eq!(reader.get(i as u64), v, "bpv {bpv} index {i}");
              }
          }
      }

      #[test]
      fn direct_reader_known_layouts() {
          // bpv 8: plain LE bytes
          let r = DirectReader::new(&[1, 2, 255], 8, 0).unwrap();
          assert_eq!(r.get(0), 1);
          assert_eq!(r.get(1), 2);
          assert_eq!(r.get(2), 255);
          // bpv 1: 3 bits packed low-first in one byte
          let r = DirectReader::new(&[0b101], 1, 0).unwrap();
          assert_eq!(r.get(0), 1);
          assert_eq!(r.get(1), 0);
          assert_eq!(r.get(2), 1);
          // bpv 12: l1 | l2<<12 in 3 bytes (+ padding)
          let r = DirectReader::new(&[0xbc, 0xfa, 0xde, 0x00], 12, 0).unwrap();
          assert_eq!(r.get(0), 0xabc);
          assert_eq!(r.get(1), 0xdef);
          // bpv 16 LE shorts
          let r = DirectReader::new(&[1, 2], 16, 0).unwrap();
          assert_eq!(r.get(0), 0x0201);
          // nonzero base offset
          let r = DirectReader::new(&[0xFF, 1, 2], 16, 1).unwrap();
          assert_eq!(r.get(0), 0x0201);
          // unsupported bpv rejected
          assert!(DirectReader::new(&[0u8; 8], 3, 0).is_err());
      }
  ```

- [ ] **Step 3.2: 跑测试确认失败**

  ```
  $ cargo test -p codec-lucene9 packed:: 2>&1 | tail -5
  error[E0433]: failed to resolve: use of undeclared type `DirectMonotonicReader`
  ```

- [ ] **Step 3.3: 最小实现** — `crates/codec-lucene9/src/packed.rs` 在 `direct_monotonic_write` 之后、`#[cfg(test)]` 之前追加：

  ```rust
  // ---------------------------------------------------------------------------
  // Read side (DirectReader / DirectMonotonicReader), mirrors of the writers.
  // ---------------------------------------------------------------------------

  /// DirectReader.getInstance + per-bpv `get` (DirectReader.java:58-91,199-461):
  /// values are packed LSB-first in the byte stream; a little-endian container
  /// read at the value's bit offset, shifted and masked, yields the value.
  /// The single generic form below covers every supported bpv (Java's
  /// specializations read 1/2/4/8-byte containers; a clamped 8-byte LE window
  /// is semantically identical given DirectWriter.finish's container padding,
  /// DirectWriter.java:156-173).
  pub struct DirectReader<'a> {
      data: &'a [u8],
      bits_per_value: u32,
      /// Byte offset of bit 0 of value 0.
      offset: u64,
  }

  impl<'a> DirectReader<'a> {
      pub fn new(data: &'a [u8], bits_per_value: u32, offset: u64) -> io::Result<Self> {
          if !SUPPORTED_BITS_PER_VALUE.contains(&bits_per_value) {
              return Err(io::Error::new(
                  io::ErrorKind::InvalidData,
                  format!("unsupported bitsPerValue {bits_per_value} (DirectReader.java:89)"),
              ));
          }
          Ok(DirectReader {
              data,
              bits_per_value,
              offset,
          })
      }

      /// DirectReader.get: bit position = offset*8 + index*bpv, LSB-first.
      pub fn get(&self, index: u64) -> u64 {
          let bpv = self.bits_per_value as u64;
          let bit_offset = self.offset * 8 + index * bpv;
          let byte_offset = (bit_offset / 8) as usize;
          let shift = (bit_offset % 8) as u32;
          let mut buf = [0u8; 8];
          let available = self.data.len() - byte_offset;
          let take = available.min(8);
          buf[..take].copy_from_slice(&self.data[byte_offset..byte_offset + take]);
          let raw = u64::from_le_bytes(buf) >> shift;
          if bpv == 64 {
              raw
          } else {
              raw & ((1u64 << bpv) - 1)
          }
      }
  }

  /// DirectMonotonicReader (DirectMonotonicReader.java): monotone sequence
  /// reconstructed per block as `min + (long)(avgInc * blockIndex) + delta`
  /// (:160-165). Meta records are 21 bytes each: LE long min, LE int
  /// Float.floatToIntBits(avgInc), LE long data offset, byte bpv
  /// (loadMeta :84-100).
  pub struct DirectMonotonicReader<'a> {
      mins: Vec<i64>,
      avgs: Vec<f32>,
      offsets: Vec<u64>,
      bpvs: Vec<u8>,
      data: &'a [u8],
      block_shift: u32,
  }

  impl<'a> DirectMonotonicReader<'a> {
      /// Bytes per meta record (DirectMonotonicReader.loadMeta :88-96).
      pub const META_RECORD_BYTES: usize = 21;

      pub fn new(
          meta: &[u8],
          data: &'a [u8],
          num_values: usize,
          block_shift: u32,
      ) -> io::Result<Self> {
          // Meta constructor (:56-67): numBlocks = ceil(numValues >>> blockShift)
          let num_blocks = if num_values == 0 {
              0
          } else {
              (num_values - 1) >> block_shift
          } + 1;
          if meta.len() < num_blocks * Self::META_RECORD_BYTES {
              return Err(io::Error::new(
                  io::ErrorKind::InvalidData,
                  format!(
                      "DirectMonotonic meta too short: {} bytes for {num_blocks} blocks",
                      meta.len()
                  ),
              ));
          }
          let mut mins = Vec::with_capacity(num_blocks);
          let mut avgs = Vec::with_capacity(num_blocks);
          let mut offsets = Vec::with_capacity(num_blocks);
          let mut bpvs = Vec::with_capacity(num_blocks);
          for b in 0..num_blocks {
              let pos = b * Self::META_RECORD_BYTES;
              mins.push(i64::from_le_bytes(meta[pos..pos + 8].try_into().unwrap()));
              avgs.push(f32::from_bits(u32::from_le_bytes(
                  meta[pos + 8..pos + 12].try_into().unwrap(),
              )));
              offsets.push(u64::from_le_bytes(meta[pos + 12..pos + 20].try_into().unwrap()));
              bpvs.push(meta[pos + 20]);
          }
          Ok(DirectMonotonicReader {
              mins,
              avgs,
              offsets,
              bpvs,
              data,
              block_shift,
          })
      }

      /// DirectMonotonicReader.get (:160-165): min + (long)(avgInc * blockIndex)
      /// + delta, with Java's float multiply + truncate-toward-zero semantics
      /// and wrapping long arithmetic. bpv==0 blocks read as zero (:113-114).
      pub fn get(&self, index: u64) -> u64 {
          let block = (index >> self.block_shift) as usize;
          let block_index = index & ((1u64 << self.block_shift) - 1);
          let bpv = self.bpvs[block];
          let delta = if bpv == 0 {
              0
          } else {
              DirectReader {
                  data: self.data,
                  bits_per_value: bpv as u32,
                  offset: self.offsets[block],
              }
              .get(block_index)
          };
          self.mins[block]
              .wrapping_add((self.avgs[block] * block_index as f32) as i64)
              .wrapping_add(delta as i64) as u64
      }
  }
  ```

- [ ] **Step 3.4: 跑测试确认通过**

  ```
  $ cargo test -p codec-lucene9 packed:: 2>&1 | tail -3
  test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  ```
  （既有 7 个 round-trip/layout 测试走生产 reader + 2 个新测试。）

- [ ] **Step 3.5: 提交**

  ```
  git add crates/codec-lucene9/src/packed.rs
  git commit -m "feat: DirectReader/DirectMonotonicReader (packed read side)"
  ```

---

## Task 4: segments_N / .si / .fnm 解析读

**Files:**
- Modify: `crates/codec-lucene9/src/segment_infos.rs`（追加 `read_commit` / `read_latest` + 测试）
- Modify: `crates/codec-lucene9/src/segment_info.rs`（追加 `SegmentInfo::read` + 测试）
- Modify: `crates/codec-lucene9/src/field_infos.rs`（追加 `FieldInfos::read` / `by_number` + `FORMAT_START` 常量 + 测试）
- Test: 上述三个文件的 `#[cfg(test)]` 模块

**Interfaces:**
- Consumes: T1 的 `DataInput` / `ChecksumIndexInput` / `FSDirectory::open_checksum_input` / `codec_util::{check_header, check_index_header, check_index_header_suffix, check_footer, read_be_int, read_be_long, corrupt}`；写侧 `SegmentInfos::commit` / `SegmentInfo::write` / `FieldInfos::write` / `file_name_from_generation`。
- Produces:
  ```rust
  // segment_infos.rs
  impl SegmentInfos {
      pub fn read_commit(dir: &FSDirectory, generation: i64) -> io::Result<SegmentInfos>;
      pub fn read_latest(dir: &FSDirectory) -> io::Result<(SegmentInfos, i64)>;
  }
  // segment_info.rs
  impl SegmentInfo {
      pub fn read(dir: &FSDirectory, name: &str, expected_id: &[u8; 16], suffix: &str) -> io::Result<SegmentInfo>;
  }
  // field_infos.rs
  impl FieldInfos {
      pub fn read(dir: &FSDirectory, segment: &str, segment_id: &[u8; 16], suffix: &str) -> io::Result<FieldInfos>;
      pub fn by_number(&self, number: i32) -> Option<&FieldInfo>;
  }
  ```

### Steps

- [ ] **Step 4.1: 写失败测试** — `segment_info.rs` 的 `mod tests` 追加：

  ```rust
      #[test]
      fn si_round_trip() {
          let root = temp_dir("si_read");
          let dir = FSDirectory::open(&root).unwrap();
          let mut si = SegmentInfo::new("_0", [7u8; 16], 42);
          si.files.insert("_0.si".to_string());
          si.diagnostics.insert("os".to_string(), "Linux".to_string());
          si.attributes.insert(
              STORED_FIELDS_MODE_ATTRIBUTE.to_string(),
              STORED_FIELDS_MODE_BEST_SPEED.to_string(),
          );
          si.write(&dir, "").unwrap();

          let back = SegmentInfo::read(&dir, "_0", &[7u8; 16], "").unwrap();
          assert_eq!(back.name, "_0");
          assert_eq!(back.id, [7u8; 16]);
          assert_eq!(back.version, (9, 12, 3));
          assert_eq!(back.min_version, Some((9, 12, 3)));
          assert_eq!(back.doc_count, 42);
          assert!(!back.is_compound_file && !back.has_blocks);
          assert_eq!(back.diagnostics.get("os").unwrap(), "Linux");
          assert!(back.files.contains("_0.si"));
          assert_eq!(
              back.attributes.get(STORED_FIELDS_MODE_ATTRIBUTE).unwrap(),
              STORED_FIELDS_MODE_BEST_SPEED
          );
          // wrong id rejected
          assert!(SegmentInfo::read(&dir, "_0", &[9u8; 16], "").is_err());
          fs::remove_dir_all(&root).unwrap();
      }
  ```

  `field_infos.rs` 的 `mod tests` 追加：

  ```rust
      fn temp_dir(tag: &str) -> std::path::PathBuf {
          let dir = std::env::temp_dir().join(format!(
              "codec-lucene9-fnm-{}-{}",
              tag,
              std::process::id()
          ));
          let _ = std::fs::remove_dir_all(&dir);
          dir
      }

      #[test]
      fn fnm_round_trip() {
          let root = temp_dir("fnm_read");
          let dir = crate::directory::FSDirectory::open(&root).unwrap();
          let mut indexed = FieldInfo::stored("message", 0);
          indexed.omit_norms = true;
          indexed.index_options = IndexOptions::DocsAndFreqs;
          let mut point = FieldInfo::stored("timestamp", 1);
          point.point_dimension_count = 1;
          point.point_index_dimension_count = 1;
          point.point_num_bytes = 8;
          point.doc_values_type = DocValuesType::Numeric;
          let fis = FieldInfos::new(vec![indexed, point]);
          fis.write(&dir, "_0", &[3u8; 16], "").unwrap();

          let back = FieldInfos::read(&dir, "_0", &[3u8; 16], "").unwrap();
          assert_eq!(back.fields.len(), 2);
          let f0 = back.by_name("message").unwrap();
          assert_eq!(f0.number, 0);
          assert!(f0.omit_norms);
          assert!(matches!(f0.index_options, IndexOptions::DocsAndFreqs));
          let f1 = back.by_number(1).unwrap();
          assert_eq!(f1.name, "timestamp");
          assert_eq!(f1.point_dimension_count, 1);
          assert_eq!(f1.point_num_bytes, 8);
          assert!(matches!(f1.doc_values_type, DocValuesType::Numeric));
          assert!(back.by_number(2).is_none());
          std::fs::remove_dir_all(&root).unwrap();
      }
  ```

  `segment_infos.rs` 的 `mod tests` 追加：

  ```rust
      fn temp_dir(tag: &str) -> std::path::PathBuf {
          let dir = std::env::temp_dir().join(format!(
              "codec-lucene9-sis-{}-{}",
              tag,
              std::process::id()
          ));
          let _ = std::fs::remove_dir_all(&dir);
          dir
      }

      fn commit_one(dir: &FSDirectory, infos: &mut SegmentInfos, name: &str, id: [u8; 16], gen: i64) {
          let mut si = SegmentInfo::new(name, id, 10);
          si.files.insert(format!("{name}.si"));
          si.write(dir, "").unwrap();
          infos.segments.push(SegmentCommitInfo::new(si, id));
          infos.commit(dir, gen).unwrap();
      }

      #[test]
      fn segments_round_trip_and_latest_generation() {
          let root = temp_dir("read_commit");
          let dir = FSDirectory::open(&root).unwrap();
          let mut infos = SegmentInfos::new();
          infos.user_data.insert("commit".to_string(), "first".to_string());
          commit_one(&dir, &mut infos, "_0", [1u8; 16], 1);
          commit_one(&dir, &mut infos, "_1", [2u8; 16], 2);

          // read_latest picks the highest generation and parses both segments
          let (back, gen) = SegmentInfos::read_latest(&dir).unwrap();
          assert_eq!(gen, 2);
          assert_eq!(back.segments.len(), 2);
          assert_eq!(back.segments[0].info.name, "_0");
          assert_eq!(back.segments[0].info.doc_count, 10);
          assert_eq!(back.segments[0].id, Some([1u8; 16]));
          assert_eq!(back.segments[1].info.name, "_1");
          assert_eq!(back.segments[1].del_gen, -1);
          assert_eq!(back.segments[1].field_infos_gen, -1);
          assert_eq!(back.segments[1].doc_values_gen, -1);
          assert_eq!(back.user_data.get("commit").unwrap(), "first");

          // read_commit reads a specific older generation
          let gen1 = SegmentInfos::read_commit(&dir, 1).unwrap();
          assert_eq!(gen1.segments.len(), 1);
          assert_eq!(gen1.segments[0].info.name, "_0");
          std::fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn corrupted_commit_rejected() {
          let root = temp_dir("corrupt_commit");
          let dir = FSDirectory::open(&root).unwrap();
          let mut infos = SegmentInfos::new();
          commit_one(&dir, &mut infos, "_0", [1u8; 16], 1);
          // flip a byte in the middle of segments_1
          let path = root.join("segments_1");
          let mut bytes = std::fs::read(&path).unwrap();
          let mid = bytes.len() / 2;
          bytes[mid] ^= 0xFF;
          std::fs::write(&path, bytes).unwrap();
          assert!(SegmentInfos::read_commit(&dir, 1).is_err());
          std::fs::remove_dir_all(&root).unwrap();
      }
  ```

  注意：`commit_one` 内 `si.write(dir, "")` 需要 .si 文件内容合法（name/id 匹配）；`SegmentCommitInfo::new` 已有。`segment_infos.rs` 测试模块顶部需 `use crate::segment_info::SegmentInfo;`（文件顶部已有，测试模块 `use super::*` 继承）。

- [ ] **Step 4.2: 跑测试确认失败**

  ```
  $ cargo test -p codec-lucene9 segment_info 2>&1 | tail -5
  error[E0599]: no function or associated item named `read` found for struct `SegmentInfo`
  ```

- [ ] **Step 4.3: 最小实现** —

  `crates/codec-lucene9/src/segment_info.rs`：`use` 行追加 `use crate::codec_util::{check_footer, check_index_header, corrupt};`（替换现有 `use crate::codec_util::{write_footer, write_index_header};`），并在 `impl SegmentInfo` 内 `write` 之后追加：

  ```rust
      /// Lucene99SegmentInfoFormat.read/parseSegmentInfo (:91-168): mirror of
      /// [`SegmentInfo::write`]. The index sort must be absent (we never
      /// write one).
      pub fn read(
          dir: &FSDirectory,
          name: &str,
          expected_id: &[u8; 16],
          suffix: &str,
      ) -> io::Result<SegmentInfo> {
          let file_name = format!("{name}{suffix}.{EXTENSION}");
          let mut input = dir.open_checksum_input(&file_name)?;
          check_index_header(
              &mut input,
              CODEC_NAME,
              VERSION_CURRENT,
              VERSION_CURRENT,
              expected_id,
              suffix,
          )?;
          let version = (input.read_int()?, input.read_int()?, input.read_int()?);
          let min_version = match input.read_byte()? {
              0 => None,
              1 => Some((input.read_int()?, input.read_int()?, input.read_int()?)),
              b => return Err(corrupt(format!("invalid minVersion marker {b}"))),
          };
          let doc_count = input.read_int()?;
          if doc_count < 0 {
              return Err(corrupt(format!("invalid docCount {doc_count}")));
          }
          let is_compound_file = input.read_byte()? == YES as u8;
          let has_blocks = input.read_byte()? == YES as u8;
          let diagnostics = input.read_map_of_strings()?;
          let files = input.read_set_of_strings()?;
          let attributes = input.read_map_of_strings()?;
          let num_sort_fields = input.read_vint()?;
          if num_sort_fields != 0 {
              return Err(corrupt(format!(
                  "unsupported: index sort ({num_sort_fields} fields)"
              )));
          }
          check_footer(&mut input)?;
          Ok(SegmentInfo {
              name: name.to_string(),
              id: *expected_id,
              version,
              min_version,
              doc_count,
              is_compound_file,
              has_blocks,
              diagnostics,
              files,
              attributes,
          })
      }
  ```

  `crates/codec-lucene9/src/field_infos.rs`：现有 `use crate::codec_util::{write_footer, write_index_header};` 替换为 `use crate::codec_util::{check_footer, check_index_header, corrupt, write_footer, write_index_header};`；`FORMAT_CURRENT` 旁加 `const FORMAT_START: u32 = 0; // :423`；`impl FieldInfos` 内追加 `by_number` 与 `read`；文件尾部追加两个私有转换函数：

  ```rust
      pub fn by_number(&self, number: i32) -> Option<&FieldInfo> {
          self.fields.iter().find(|f| f.number == number)
      }

      /// Lucene94FieldInfosFormat.read (:127-234): mirror of
      /// [`FieldInfos::write`].
      pub fn read(
          dir: &FSDirectory,
          segment: &str,
          segment_id: &[u8; 16],
          suffix: &str,
      ) -> io::Result<FieldInfos> {
          let file_name = format!("{segment}{suffix}.{EXTENSION}");
          let mut input = dir.open_checksum_input(&file_name)?;
          check_index_header(
              &mut input,
              CODEC_NAME,
              FORMAT_START,
              FORMAT_CURRENT,
              segment_id,
              suffix,
          )?;
          let size = input.read_vint()?;
          if size < 0 {
              return Err(corrupt(format!("invalid field count {size}")));
          }
          let mut fields = Vec::with_capacity(size as usize);
          for _ in 0..size {
              let name = input.read_string()?;
              let number = input.read_vint()?;
              let bits = input.read_byte()?;
              if bits & 0xE0 != 0 {
                  return Err(corrupt(format!("invalid field bits {bits:#x}")));
              }
              let index_options = index_options_from_byte(input.read_byte()?)?;
              let doc_values_type = doc_values_type_from_byte(input.read_byte()?)?;
              let doc_values_gen = input.read_long()?;
              let attributes = input.read_map_of_strings()?;
              let point_dimension_count = input.read_vint()?;
              let (point_index_dimension_count, point_num_bytes) = if point_dimension_count != 0 {
                  (input.read_vint()?, input.read_vint()?)
              } else {
                  (0, 0)
              };
              let vector_dimension = input.read_vint()?;
              let vector_encoding = match input.read_byte()? {
                  0 => VectorEncoding::Byte,
                  1 => VectorEncoding::Float32,
                  b => return Err(corrupt(format!("invalid vector encoding {b}"))),
              };
              let vector_similarity = match input.read_byte()? {
                  0 => VectorSimilarity::Euclidean,
                  1 => VectorSimilarity::DotProduct,
                  2 => VectorSimilarity::Cosine,
                  3 => VectorSimilarity::MaximumInnerProduct,
                  b => return Err(corrupt(format!("invalid vector similarity {b}"))),
              };
              fields.push(FieldInfo {
                  name,
                  number,
                  store_termvector: bits & STORE_TERMVECTOR != 0,
                  omit_norms: bits & OMIT_NORMS != 0,
                  store_payloads: bits & STORE_PAYLOADS != 0,
                  soft_deletes: bits & SOFT_DELETES_FIELD != 0,
                  parent_field: bits & PARENT_FIELD_FIELD != 0,
                  index_options,
                  doc_values_type,
                  doc_values_gen,
                  attributes,
                  point_dimension_count,
                  point_index_dimension_count,
                  point_num_bytes,
                  vector_dimension,
                  vector_encoding,
                  vector_similarity,
              });
          }
          check_footer(&mut input)?;
          Ok(FieldInfos { fields })
      }
  ```

  ```rust
  /// getIndexOptions (:349-365).
  fn index_options_from_byte(b: u8) -> io::Result<IndexOptions> {
      match b {
          0 => Ok(IndexOptions::None),
          1 => Ok(IndexOptions::Docs),
          2 => Ok(IndexOptions::DocsAndFreqs),
          3 => Ok(IndexOptions::DocsAndFreqsAndPositions),
          4 => Ok(IndexOptions::DocsAndFreqsAndPositionsAndOffsets),
          _ => Err(corrupt(format!("invalid index options {b}"))),
      }
  }

  /// getDocValuesType (:263-280).
  fn doc_values_type_from_byte(b: u8) -> io::Result<DocValuesType> {
      match b {
          0 => Ok(DocValuesType::None),
          1 => Ok(DocValuesType::Numeric),
          2 => Ok(DocValuesType::Binary),
          3 => Ok(DocValuesType::Sorted),
          4 => Ok(DocValuesType::SortedSet),
          5 => Ok(DocValuesType::SortedNumeric),
          _ => Err(corrupt(format!("invalid doc values type {b}"))),
      }
  }
  ```

  `crates/codec-lucene9/src/segment_infos.rs`：`use` 行追加 `check_footer, check_header, check_index_header_id, check_index_header_suffix, corrupt, read_be_int, read_be_long`（并入现有 `use crate::codec_util::{...};`），`impl SegmentInfos` 内 `commit` 之后追加：

  ```rust
      /// SegmentInfos.readCommit (:327-389) + parseSegmentInfos (:391-519).
      /// Reads `segments_<base36 generation>`; the commit id is parsed but not
      /// validated (:341-342 reads it without comparison). Enforces the spec §1
      /// premise: no deletes, no field-info/docvalues updates.
      pub fn read_commit(dir: &FSDirectory, generation: i64) -> io::Result<SegmentInfos> {
          let file_name = file_name_from_generation(SEGMENTS, generation);
          let mut input = dir.open_checksum_input(&file_name)?;
          let _format = check_header(&mut input, CODEC_NAME, 7, VERSION_CURRENT)?; // VERSION_70..=VERSION_86
          let mut commit_id = [0u8; 16];
          input.read_bytes(&mut commit_id)?; // :341-342, unverified by design
          check_index_header_suffix(&mut input, &to_base36(generation))?; // :343
          let _lucene_version = (input.read_vint()?, input.read_vint()?, input.read_vint()?); // :345-346
          let index_created_version_major = input.read_vint()?; // :347
          let version = read_be_long(&mut input)? as i64; // :393
          let counter = input.read_vlong()?; // :395-399
          let num_segments = read_be_int(&mut input)? as usize; // :400
          let min_segment_version = if num_segments > 0 {
              Some((input.read_vint()?, input.read_vint()?, input.read_vint()?)) // :405-410
          } else {
              None
          };
          let mut segments = Vec::with_capacity(num_segments);
          for _ in 0..num_segments {
              let seg_name = input.read_string()?; // :414
              let mut seg_id = [0u8; 16];
              input.read_bytes(&mut seg_id)?; // :415-416
              let codec = input.read_string()?; // :417
              if codec != "Lucene912" {
                  return Err(corrupt(format!("unsupported codec {codec}")));
              }
              let del_gen = read_be_long(&mut input)? as i64; // :422
              let del_count = read_be_int(&mut input)? as i32; // :423
              let field_infos_gen = read_be_long(&mut input)? as i64; // :428
              let doc_values_gen = read_be_long(&mut input)? as i64; // :429
              let soft_del_count = read_be_int(&mut input)? as i32; // :430
              let id = match input.read_byte()? {
                  // :441-457
                  1 => {
                      let mut b = [0u8; 16];
                      input.read_bytes(&mut b)?;
                      Some(b)
                  }
                  0 => None,
                  b => return Err(corrupt(format!("invalid SCI id marker {b}"))),
              };
              let field_infos_files = input.read_set_of_strings()?; // :460
              let num_dv_fields = read_be_int(&mut input)?; // :462-471
              let mut doc_values_updates = BTreeMap::new();
              for _ in 0..num_dv_fields {
                  let field_number = read_be_int(&mut input)? as i32;
                  let files = input.read_set_of_strings()?;
                  doc_values_updates.insert(field_number, files);
              }
              // spec §1 premise: our own indexes only (no deletes/updates)
              if del_gen != -1
                  || del_count != 0
                  || field_infos_gen != -1
                  || doc_values_gen != -1
                  || soft_del_count != 0
                  || !doc_values_updates.is_empty()
              {
                  return Err(corrupt(
                      "unsupported: live docs / field-info / docvalues updates (spec §1)",
                  ));
              }
              // codec.segmentInfoFormat().read (:418-422): the .si file is
              // parsed here, between codec name and delGen in stream order.
              let info = SegmentInfo::read(dir, &seg_name, &seg_id, "")?;
              segments.push(SegmentCommitInfo {
                  info,
                  del_gen,
                  del_count,
                  field_infos_gen,
                  doc_values_gen,
                  soft_del_count,
                  id,
                  field_infos_files,
                  doc_values_updates,
              });
          }
          let user_data = input.read_map_of_strings()?; // :508
          check_footer(&mut input)?; // :379-387
          Ok(SegmentInfos {
              version,
              counter,
              index_created_version_major,
              min_segment_version,
              segments,
              user_data,
          })
      }

      /// SegmentInfos.readLatestCommit (:539-557) over
      /// getLastCommitGeneration (:201-215): highest base36 generation among
      /// `segments_*` files (excluding `segments.gen` and pending commits).
      pub fn read_latest(dir: &FSDirectory) -> io::Result<(SegmentInfos, i64)> {
          let mut best: Option<i64> = None;
          for name in dir.list_all()? {
              if !name.starts_with(SEGMENTS) || name == "segments.gen" {
                  continue;
              }
              let Some(gen_str) = name[SEGMENTS.len()..].strip_prefix('_') else {
                  continue; // bare "segments" is not a commit file
              };
              // generationFromSegmentsFileName (:254-266)
              let Ok(gen) = i64::from_str_radix(gen_str, BASE36) else {
                  continue;
              };
              best = Some(best.map_or(gen, |b: i64| b.max(gen)));
          }
          let generation = best.ok_or_else(|| {
              io::Error::new(io::ErrorKind::NotFound, "no segments_N commit found")
          })?;
          Ok((Self::read_commit(dir, generation)?, generation))
      }
  ```

  `i64::from_str_radix(gen_str, BASE36)`：`BASE36` 是 `u32` 常量（`= 36`），`from_str_radix` 第二参数为 u32 ✓。

- [ ] **Step 4.4: 跑测试确认通过**

  ```
  $ cargo test -p codec-lucene9 segment 2>&1 | tail -3
  test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  $ cargo test -p codec-lucene9 field_infos 2>&1 | tail -3
  test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  ```

- [ ] **Step 4.5: 提交**

  ```
  git add crates/codec-lucene9/src/segment_infos.rs crates/codec-lucene9/src/segment_info.rs crates/codec-lucene9/src/field_infos.rs
  git commit -m "feat: segments_N/.si/.fnm parse readers"
  ```

---

## Task 5: FST 读（fst.rs 追加）

**Files:**
- Modify: `crates/codec-lucene9/src/fst.rs`（追加 `FstMetadata` / `FstReader` / `FstArc` + 测试）
- Test: `crates/codec-lucene9/src/fst.rs` 的 `#[cfg(test)]` 模块（既有测试保持不动，新增生产 reader 测试）

**Interfaces:**
- Consumes: T1 的 `DataInput` / `IndexInput` / `codec_util::{check_header, corrupt}`；写侧 `FstCompiler` / `Fst::write_metadata`（round-trip 对拍）；本文件私有常量 `BIT_*` / `FINAL_END_NODE` / `NON_FINAL_END_NODE` / `FILE_FORMAT_NAME` / `VERSION_CURRENT`。
- Produces:
  ```rust
  #[derive(Clone)]
  pub struct FstMetadata {
      pub start_node: u64,
      pub num_bytes: u64,
      pub empty_output: Option<Vec<u8>>,
  }
  impl FstMetadata {
      pub fn read(input: &mut impl DataInput) -> io::Result<FstMetadata>;
  }
  pub struct FstReader { .. }
  impl FstReader {
      pub fn new(bytes: Vec<u8>, metadata: &FstMetadata) -> FstReader;
      pub fn empty_output(&self) -> Option<&[u8]>;
      pub fn lookup(&self, input: &[u8]) -> io::Result<Option<Vec<u8>>>;
      pub fn trace_path(&self, input: &[u8]) -> io::Result<Vec<(usize, Vec<u8>)>>;
  }
  #[derive(Clone, Debug)]
  pub struct FstArc {
      pub label: u8,
      pub output: Option<Vec<u8>>,
      pub final_output: Option<Vec<u8>>,
      pub is_final: bool,
      pub target: i64,
  }
  ```

### Steps

- [ ] **Step 5.1: 写失败测试** — 追加到 `fst.rs` 的 `mod tests`：

  ```rust
      fn fst_reader(entries: &[(&[u8], Option<&[u8]>)]) -> (FstReader, Vec<u8>) {
          let mut compiler = FstCompiler::new();
          for (input, output) in entries {
              compiler.add(input, *output);
          }
          let fst = compiler.finish();
          let mut out = ChecksumIndexOutput::new(IndexOutput::in_memory());
          fst.write_metadata(&mut out).unwrap();
          let meta_bytes = out.into_bytes();
          let metadata =
              FstMetadata::read(&mut IndexInput::in_memory(meta_bytes)).unwrap();
          assert_eq!(metadata.start_node, fst.start_node());
          assert_eq!(metadata.num_bytes, fst.num_bytes());
          (
              FstReader::new(fst.bytes().to_vec(), &metadata),
              fst.bytes().to_vec(),
          )
      }

      #[test]
      fn reader_lookup_round_trip() {
          let entries: &[(&[u8], Option<&[u8]>)] = &[
              (b"a", Some(b"\x01")),
              (b"ab", Some(b"\x02\x03")),
              (b"abc", Some(b"\x02")),
              (b"b", Some(b"\x05")),
              (b"ca", None),
              (b"cabd", Some(b"\x09\x09\x09")),
          ];
          let (reader, _) = fst_reader(entries);
          for (input, output) in entries {
              assert_eq!(
                  reader.lookup(input).unwrap().as_deref(),
                  Some(output.unwrap_or(&[]).as_slice()),
                  "lookup {:?}",
                  String::from_utf8_lossy(input)
              );
          }
          assert_eq!(reader.lookup(b"").unwrap(), None);
          assert_eq!(reader.lookup(b"ac").unwrap(), None);
          assert_eq!(reader.lookup(b"abb").unwrap(), None);
          assert_eq!(reader.lookup(b"d").unwrap(), None);
      }

      #[test]
      fn reader_metadata_empty_output() {
          let (reader, bytes) = fst_reader(&[(b"", Some(b"xy"))]);
          assert_eq!(bytes, vec![0u8]);
          assert_eq!(reader.empty_output(), Some(&b"xy"[..]));
          assert_eq!(reader.lookup(b"").unwrap(), Some(b"xy".to_vec()));
          assert_eq!(reader.lookup(b"a").unwrap(), None);
      }

      #[test]
      fn reader_trace_path() {
          // block-tree 使用方式：输出 = 块指针编码，沿路径的 final arc 给出候选帧
          let entries: &[(&[u8], Option<&[u8]>)] = &[
              (b"", Some(b"R")),
              (b"ab", Some(b"X")),
              (b"abc", Some(b"Y")),
              (b"b", Some(b"Z")),
          ];
          let (reader, _) = fst_reader(entries);
          // "abc" 全路径：depth 2 ("ab") 与 depth 3 ("abc") 两个 final
          assert_eq!(
              reader.trace_path(b"abc").unwrap(),
              vec![(2usize, b"X".to_vec()), (3usize, b"Y".to_vec())]
          );
          // "abd" 走到 depth 2 后无 arc：只有 ("ab", X)
          assert_eq!(reader.trace_path(b"abd").unwrap(), vec![(2usize, b"X".to_vec())]);
          // "c" 第一个字节就无 arc：空
          assert_eq!(reader.trace_path(b"c").unwrap(), Vec::<(usize, Vec<u8>)>::new());
      }

      #[test]
      fn reader_generated_round_trip() {
          // deterministic xorshift64* PRNG, same generator as the write-side test
          let mut state = 0x243F6A8885A308D3u64;
          let mut rand = move || {
              state ^= state << 13;
              state ^= state >> 7;
              state ^= state << 17;
              state
          };
          let mut entries: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
          for _ in 0..2000 {
              let len = 1 + (rand() % 12) as usize;
              let input: Vec<u8> = (0..len).map(|_| b'a' + (rand() % 6) as u8).collect();
              let olen = (rand() % 9) as usize;
              let output = if olen == 0 {
                  None
              } else {
                  Some((0..olen).map(|_| (rand() % 256) as u8).collect::<Vec<u8>>())
              };
              entries.push((input, output));
          }
          entries.sort_by(|a, b| a.0.cmp(&b.0));
          entries.dedup_by(|a, b| a.0 == b.0);
          let refs: Vec<(&[u8], Option<&[u8]>)> = entries
              .iter()
              .map(|(i, o)| (i.as_slice(), o.as_deref()))
              .collect();
          let (reader, _) = fst_reader(&refs);
          for (input, output) in &entries {
              assert_eq!(
                  reader.lookup(input).unwrap().as_deref(),
                  Some(output.clone().unwrap_or_default().as_slice())
              );
          }
          assert_eq!(reader.lookup(b"z").unwrap(), None);
      }
  ```

- [ ] **Step 5.2: 跑测试确认失败**

  ```
  $ cargo test -p codec-lucene9 fst:: 2>&1 | tail -5
  error[E0433]: failed to resolve: use of undeclared type `FstReader`
  ```

- [ ] **Step 5.3: 最小实现** — `crates/codec-lucene9/src/fst.rs`：`use` 行改为 `use crate::codec_util::{self, check_header, corrupt, write_be_int};` 与 `use crate::io::{ChecksumIndexOutput, DataInput, IndexInput};`，在 `impl Fst` 之后、`#[cfg(test)]` 之前追加：

  ```rust
  // ---------------------------------------------------------------------------
  // Read side (FST.readMetadata :455-500, readArc :943-991, linear-scan
  // findTargetArc :1100-1126). Our writer only produces unpacked,
  // variable-length-arc nodes (allowFixedLengthArcs == false), so the reader
  // implements exactly that node shape.
  // ---------------------------------------------------------------------------

  /// FST metadata (FST.FSTMetadata), parsed from the .tmd stream right after a
  /// field's indexStartFP (FieldReader constructor :91).
  #[derive(Clone)]
  pub struct FstMetadata {
      pub start_node: u64,
      pub num_bytes: u64,
      pub empty_output: Option<Vec<u8>>,
  }

  impl FstMetadata {
      /// FST.readMetadata (:455-500): header("FST", 6..=9) + emptyOutput flag +
      /// inputType byte (BYTE1 = 0) + VLong startNode + VLong numBytes.
      pub fn read(input: &mut impl DataInput) -> io::Result<FstMetadata> {
          check_header(input, FILE_FORMAT_NAME, 6, VERSION_CURRENT)?; // VERSION_START=6 (:114)
          let empty_output = match input.read_byte()? {
              1 => {
                  // Serialized output reversed wholesale (FSTMetadata.save
                  // :1234-1244); undo the reversal, then
                  // ByteSequenceOutputs.read (:122-132).
                  let num_bytes = input.read_vint()? as usize;
                  let mut bytes = vec![0u8; num_bytes];
                  input.read_bytes(&mut bytes)?;
                  bytes.reverse();
                  let mut cursor = IndexInput::in_memory(bytes);
                  let len = cursor.read_vint()? as usize;
                  let mut out = vec![0u8; len];
                  cursor.read_bytes(&mut out)?;
                  Some(out)
              }
              0 => None,
              b => return Err(corrupt(format!("invalid FST emptyOutput flag {b}"))),
          };
          let input_type = input.read_byte()?;
          if input_type != 0 {
              return Err(corrupt(format!(
                  "unsupported FST input type {input_type} (only BYTE1)"
              )));
          }
          let start_node = input.read_vlong()? as u64;
          let num_bytes = input.read_vlong()? as u64;
          Ok(FstMetadata {
              start_node,
              num_bytes,
              empty_output,
          })
      }
  }

  /// One arc of an unpacked node (FST.Arc).
  #[derive(Clone, Debug)]
  pub struct FstArc {
      pub label: u8,
      pub output: Option<Vec<u8>>,
      pub final_output: Option<Vec<u8>>,
      pub is_final: bool,
      pub target: i64,
  }

  /// Read side of a compiled FST image: reverse arc traversal over the
  /// variable-length node format. A node's address is the offset of its last
  /// byte; arcs are read backwards in ascending label order.
  pub struct FstReader {
      bytes: Vec<u8>,
      start_node: u64,
      empty_output: Option<Vec<u8>>,
  }

  impl FstReader {
      pub fn new(bytes: Vec<u8>, metadata: &FstMetadata) -> FstReader {
          debug_assert_eq!(bytes.len() as u64, metadata.num_bytes);
          FstReader {
              bytes,
              start_node: metadata.start_node,
              empty_output: metadata.empty_output.clone(),
          }
      }

      /// Output of the empty string (`None` when the empty input is rejected).
      pub fn empty_output(&self) -> Option<&[u8]> {
          self.empty_output.as_deref()
      }

      /// VInts/VLongs are written low-group-first, so reading the reversed
      /// bytes from the node's end reassembles them with the first byte read
      /// holding the lowest 7 bits.
      fn read_vlong_rev(&self, pos: &mut i64) -> io::Result<u64> {
          let mut v = 0u64;
          let mut shift = 0;
          loop {
              if *pos < 0 {
                  return Err(corrupt("FST node overruns the image"));
              }
              let b = self.bytes[*pos as usize];
              *pos -= 1;
              v |= ((b & 0x7f) as u64) << shift;
              if b & 0x80 == 0 {
                  return Ok(v);
              }
              shift += 7;
              if shift >= 64 {
                  return Err(corrupt("FST: vLong too long"));
              }
          }
      }

      /// ByteSequenceOutputs.read (:122-132) over reversed bytes.
      fn read_output_rev(&self, pos: &mut i64) -> io::Result<Vec<u8>> {
          let len = self.read_vlong_rev(pos)? as usize;
          let end = *pos as usize;
          if len > end + 1 {
              return Err(corrupt("FST output overruns the image"));
          }
          let start = end + 1 - len;
          let mut v = self.bytes[start..=end].to_vec();
          v.reverse();
          *pos = start as i64 - 1;
          Ok(v)
      }

      /// FST.readArc (:943-991) over a whole node: arcs in ascending label
      /// order plus the position just below the node, which is what
      /// BIT_TARGET_NEXT resolves to (:966-990).
      fn read_node(&self, addr: u64) -> io::Result<(Vec<FstArc>, i64)> {
          if addr as usize >= self.bytes.len() {
              return Err(corrupt("FST node address out of bounds"));
          }
          let mut pos = addr as i64;
          let mut arcs = Vec::new();
          loop {
              let flags = self.bytes[pos as usize];
              pos -= 1;
              if pos < 0 {
                  return Err(corrupt("FST arc overruns the image"));
              }
              let label = self.bytes[pos as usize];
              pos -= 1;
              let output = if flags & BIT_ARC_HAS_OUTPUT != 0 {
                  Some(self.read_output_rev(&mut pos)?)
              } else {
                  None
              };
              let final_output = if flags & BIT_ARC_HAS_FINAL_OUTPUT != 0 {
                  Some(self.read_output_rev(&mut pos)?)
              } else {
                  None
              };
              let target = if flags & BIT_STOP_NODE != 0 {
                  if flags & BIT_FINAL_ARC != 0 {
                      FINAL_END_NODE
                  } else {
                      NON_FINAL_END_NODE
                  }
              } else if flags & BIT_TARGET_NEXT != 0 {
                  i64::MIN // resolved below, once the whole node is parsed
              } else {
                  self.read_vlong_rev(&mut pos)? as i64
              };
              arcs.push(FstArc {
                  label,
                  output,
                  final_output,
                  is_final: flags & BIT_FINAL_ARC != 0,
                  target,
              });
              if flags & BIT_LAST_ARC != 0 {
                  break;
              }
          }
          for arc in &mut arcs {
              if arc.target == i64::MIN {
                  arc.target = pos;
              }
          }
          Ok((arcs, pos))
      }

      /// findTargetArc linear scan (:1100-1126): labels ascend, so the first
      /// arc with `label >= target` decides (match or miss).
      fn find_arc(arcs: &[FstArc], label: u8) -> Option<&FstArc> {
          arcs
              .iter()
              .find(|a| a.label >= label)
              .filter(|a| a.label == label)
      }

      /// FST.Util.get semantics: the full output of `input` (empty vec when
      /// the FST maps it to NO_OUTPUT), or `None` when `input` is rejected.
      pub fn lookup(&self, input: &[u8]) -> io::Result<Option<Vec<u8>>> {
          if input.is_empty() {
              return Ok(self.empty_output.clone());
          }
          let mut out = Vec::new();
          let mut node = self.start_node as i64;
          for (i, &b) in input.iter().enumerate() {
              if node <= 0 {
                  return Ok(None); // walked into an end node: not accepted
              }
              let (arcs, _) = self.read_node(node as u64)?;
              let Some(arc) = Self::find_arc(&arcs, b) else {
                  return Ok(None);
              };
              if let Some(o) = &arc.output {
                  out.extend_from_slice(o);
              }
              if i == input.len() - 1 {
                  if !arc.is_final {
                      return Ok(None);
                  }
                  if let Some(fo) = &arc.final_output {
                      out.extend_from_slice(fo);
                  }
                  return Ok(Some(out));
              }
              if arc.target <= 0 {
                  return Ok(None);
              }
              node = arc.target;
          }
          unreachable!()
      }

      /// Walks `input` from the root, returning `(bytes consumed, full output)`
      /// at every final arc on the matched path — the candidate block frames
      /// of the block-tree seek (SegmentTermsEnum.seekExact :477-545). The
      /// root frame (empty output at depth 0) is *not* included; callers add
      /// it from the field's rootCode.
      pub fn trace_path(&self, input: &[u8]) -> io::Result<Vec<(usize, Vec<u8>)>> {
          let mut frames = Vec::new();
          if self.start_node == 0 || input.is_empty() {
              return Ok(frames);
          }
          let mut out: Vec<u8> = Vec::new();
          let mut node = self.start_node as i64;
          for (i, &b) in input.iter().enumerate() {
              if node <= 0 {
                  break;
              }
              let (arcs, _) = self.read_node(node as u64)?;
              let Some(arc) = Self::find_arc(&arcs, b) else {
                  break;
              };
              if let Some(o) = &arc.output {
                  out.extend_from_slice(o);
              }
              if arc.is_final {
                  let mut full = out.clone();
                  if let Some(fo) = &arc.final_output {
                      full.extend_from_slice(fo);
                  }
                  frames.push((i + 1, full));
              }
              node = arc.target;
          }
          Ok(frames)
      }
  }
  ```

  注：`FINAL_END_NODE` / `NON_FINAL_END_NODE` / `BIT_*` 常量已存在于本文件（私有），直接复用；`IndexInput` 需加入 `use`。

- [ ] **Step 5.4: 跑测试确认通过**

  ```
  $ cargo test -p codec-lucene9 fst:: 2>&1 | tail -3
  test result: ok. 19 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  ```

- [ ] **Step 5.5: 提交**

  ```
  git add crates/codec-lucene9/src/fst.rs
  git commit -m "feat: FST read side (metadata, lookup, trace_path)"
  ```

---

## Task 6: terms dict 读（terms_read.rs 新增）

**Files:**
- Create: `crates/codec-lucene9/src/terms_read.rs`
- Modify: `crates/codec-lucene9/src/lib.rs`（`pub mod terms_read;` + re-export）
- Modify: `crates/codec-lucene9/src/postings.rs`（常量与 `file_name` 改 `pub(crate)`：`DOC_CODEC, POS_CODEC, PSM_CODEC, TERMS_CODEC, TIM_CODEC, TIP_CODEC, TMD_CODEC, POSTINGS_VERSION, BLOCKTREE_VERSION, SEGMENT_SUFFIX, OUTPUT_FLAG_IS_FLOOR, OUTPUT_FLAG_HAS_TERMS, file_name`）
- Test: `crates/codec-lucene9/src/terms_read.rs` 的 `#[cfg(test)]` 模块

**Interfaces:**
- Consumes: T1 `DataInput` / `IndexInput` / `ChecksumIndexInput` / `codec_util::{check_index_header, check_footer, check_footer_structure, corrupt}`；T4 `FieldInfos` / `FieldInfo` / `IndexOptions`；T5 `FstMetadata` / `FstReader`；T2 `read_msb_vlong` / `BLOCK_SIZE`；写侧 `PostingsWriter`（测试语料）。
- Produces:
  ```rust
  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  pub struct TermState {
      pub doc_start_fp: u64,
      pub pos_start_fp: u64,
      pub last_pos_block_offset: i64,
      pub singleton_doc_id: i64,
  }
  #[derive(Clone, Copy, Debug)]
  pub struct TermEntry {
      pub doc_freq: u32,
      pub total_term_freq: u64,
      pub state: TermState,
  }
  pub struct FieldTermsMeta {
      pub field_number: i32,
      pub num_terms: u64,
      pub root_code: Vec<u8>,
      pub sum_total_term_freq: u64,
      pub sum_doc_freq: u64,
      pub doc_count: i32,
      pub min_term: Vec<u8>,
      pub max_term: Vec<u8>,
      pub index_start_fp: u64,
  }
  pub struct TermsDict { .. }
  impl TermsDict {
      pub fn open(dir: &FSDirectory, segment: &str, segment_id: &[u8; 16], field_infos: &FieldInfos) -> io::Result<TermsDict>;
      pub fn field_meta(&self, field_number: i32) -> Option<&FieldTermsMeta>;
      pub fn seek_exact(&mut self, field: &FieldInfo, term: &[u8]) -> io::Result<Option<TermEntry>>;
  }
  ```

### Steps

- [ ] **Step 6.1: 写失败测试** — `crates/codec-lucene9/src/terms_read.rs`（先建只有 `//!` 模块文档与测试的文件；测试驱动语料直接用写侧 `PostingsWriter`）：

  ```rust
  //! Block-tree terms dictionary reader (Lucene90BlockTreeTermsReader +
  //! SegmentTermsEnum/Frame seek path, Lucene 9.12.3).
  //!
  //! Reads `_{segment}_Lucene912_0.{tim,tip,tmd}` written by
  //! [`crate::postings::PostingsWriter`]. Only the seekExact path is
  //! implemented (TermQuery); sequential term enumeration arrives with
  //! prefix/wildcard queries (search spec phase 7).

  #[cfg(test)]
  mod tests {
      use super::*;
      use crate::directory::FSDirectory;
      use crate::field_infos::{FieldInfo, FieldInfos, IndexOptions};
      use crate::postings::PostingsWriter;
      use std::fs;

      fn temp_dir(tag: &str) -> std::path::PathBuf {
          let dir = std::env::temp_dir().join(format!(
              "codec-lucene9-terms-{}-{}",
              tag,
              std::process::id()
          ));
          let _ = fs::remove_dir_all(&dir);
          dir
      }

      fn indexed(name: &str, number: i32, opts: IndexOptions) -> FieldInfo {
          FieldInfo {
              name: name.to_string(),
              number,
              omit_norms: true,
              index_options: opts,
              ..FieldInfo::stored(name, number)
          }
      }

      /// kw (DOCS): "a" df=1 singleton doc 5; "b" df=3; plus t000..t299 df=2
      /// tx (DOCS_AND_FREQS): "hello" df=200 varied freqs; "world" df=2
      fn write_segment(dir: &FSDirectory) -> FieldInfos {
          let id = [3u8; 16];
          let kw = indexed("kw", 0, IndexOptions::Docs);
          let tx = indexed("tx", 1, IndexOptions::DocsAndFreqs);
          let mut w = PostingsWriter::new(dir, "_0", &id).unwrap();
          w.start_field(&kw, 400).unwrap();
          w.write_term(b"a", &[5], &[1], None).unwrap();
          w.write_term(b"b", &[1, 4, 9], &[1, 1, 1], None).unwrap();
          for i in 0..300 {
              let t = format!("t{i:03}");
              w.write_term(t.as_bytes(), &[1, 2], &[1, 1], None).unwrap();
          }
          w.finish_field().unwrap();
          w.start_field(&tx, 400).unwrap();
          let docs: Vec<u32> = (0..200).map(|i| i * 2).collect();
          let freqs: Vec<u32> = (0..200).map(|i| (i % 7) + 1).collect();
          w.write_term(b"hello", &docs, &freqs, None).unwrap();
          w.write_term(b"world", &[3, 300], &[2, 5], None).unwrap();
          w.finish_field().unwrap();
          w.finish().unwrap();
          let fis = FieldInfos::new(vec![kw, tx]);
          fis.write(dir, "_0", &id, "").unwrap();
          fis
      }

      #[test]
      fn field_metadata_parsed() {
          let root = temp_dir("meta");
          let dir = FSDirectory::open(&root).unwrap();
          let fis = write_segment(&dir);
          let dict = TermsDict::open(&dir, "_0", &[3u8; 16], &fis).unwrap();
          let kw = dict.field_meta(0).expect("kw record");
          assert_eq!(kw.num_terms, 302);
          assert_eq!(kw.min_term, b"a");
          assert_eq!(kw.max_term, b"t299");
          assert_eq!(kw.doc_count, 400);
          let tx = dict.field_meta(1).expect("tx record");
          assert_eq!(tx.num_terms, 2);
          assert_eq!(tx.min_term, b"hello");
          assert_eq!(tx.max_term, b"world");
          assert!(dict.field_meta(2).is_none());
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn seek_exact_found_and_singleton() {
          let root = temp_dir("seek");
          let dir = FSDirectory::open(&root).unwrap();
          let fis = write_segment(&dir);
          let mut dict = TermsDict::open(&dir, "_0", &[3u8; 16], &fis).unwrap();
          let kw = fis.by_name("kw").unwrap();
          let tx = fis.by_name("tx").unwrap();

          // singleton: df==1, docID 直接存在 TermState 里
          let e = dict.seek_exact(kw, b"a").unwrap().expect("found a");
          assert_eq!(e.doc_freq, 1);
          assert_eq!(e.state.singleton_doc_id, 5);
          // 普通 term
          let e = dict.seek_exact(kw, b"b").unwrap().expect("found b");
          assert_eq!(e.doc_freq, 3);
          assert_eq!(e.state.singleton_doc_id, -1);
          // freqs 字段的 ttf
          let e = dict.seek_exact(tx, b"hello").unwrap().expect("found hello");
          assert_eq!(e.doc_freq, 200);
          let expected_ttf: u64 = (0..200).map(|i| (i % 7) + 1).sum::<u32>() as u64;
          assert_eq!(e.total_term_freq, expected_ttf);
          // 大字典全部命中（floor / 多层 block 路径）
          for i in 0..300 {
              let t = format!("t{i:03}");
              let e = dict.seek_exact(kw, t.as_bytes()).unwrap().expect("found t");
              assert_eq!(e.doc_freq, 2, "{t}");
          }
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn seek_exact_not_found() {
          let root = temp_dir("miss");
          let dir = FSDirectory::open(&root).unwrap();
          let fis = write_segment(&dir);
          let mut dict = TermsDict::open(&dir, "_0", &[3u8; 16], &fis).unwrap();
          let kw = fis.by_name("kw").unwrap();
          let tx = fis.by_name("tx").unwrap();
          // min/max 之外
          assert!(dict.seek_exact(kw, b"0").unwrap().is_none());
          assert!(dict.seek_exact(kw, b"zzz").unwrap().is_none());
          // 字典内 but 不存在
          assert!(dict.seek_exact(kw, b"t150x").unwrap().is_none());
          assert!(dict.seek_exact(kw, b"t2").unwrap().is_none());
          // 存在于别的字段不算
          assert!(dict.seek_exact(tx, b"a").unwrap().is_none());
          assert!(dict.seek_exact(tx, b"hellp").unwrap().is_none());
          // 无记录的字段号
          let ghost = indexed("ghost", 99, IndexOptions::Docs);
          assert!(dict.seek_exact(&ghost, b"a").unwrap().is_none());
          fs::remove_dir_all(&root).unwrap();
      }
  }
  ```

- [ ] **Step 6.2: 跑测试确认失败**

  ```
  $ cargo test -p codec-lucene9 terms_read:: 2>&1 | tail -5
  error[E0433]: failed to resolve: use of undeclared type `TermsDict`
  ```

- [ ] **Step 6.3: 最小实现** — `crates/codec-lucene9/src/postings.rs` 的以下行做可见性修改（逻辑不动）：

  ```rust
  pub(crate) const DOC_CODEC: &str = "Lucene912PostingsWriterDoc";
  pub(crate) const POS_CODEC: &str = "Lucene912PostingsWriterPos";
  pub(crate) const PSM_CODEC: &str = "Lucene912PostingsWriterMeta";
  pub(crate) const TERMS_CODEC: &str = "Lucene90PostingsWriterTerms";
  pub(crate) const POSTINGS_VERSION: u32 = 0;
  pub(crate) const TIM_CODEC: &str = "BlockTreeTermsDict";
  pub(crate) const TIP_CODEC: &str = "BlockTreeTermsIndex";
  pub(crate) const TMD_CODEC: &str = "BlockTreeTermsMeta";
  pub(crate) const BLOCKTREE_VERSION: u32 = 2;
  pub(crate) const SEGMENT_SUFFIX: &str = "Lucene912_0";
  pub(crate) const OUTPUT_FLAG_IS_FLOOR: u64 = 0x1;
  pub(crate) const OUTPUT_FLAG_HAS_TERMS: u64 = 0x2;
  pub(crate) fn file_name(segment: &str, ext: &str) -> String { .. }
  ```

  `crates/codec-lucene9/src/terms_read.rs` 完整实现（追加在模块文档与测试模块之间）：

  ```rust
  use std::cmp::Ordering;
  use std::io;

  use crate::codec_util::{check_footer, check_footer_structure, check_index_header, corrupt};
  use crate::directory::FSDirectory;
  use crate::field_infos::{FieldInfo, FieldInfos, IndexOptions};
  use crate::fst::{FstMetadata, FstReader};
  use crate::io::{DataInput, IndexInput};
  use crate::postings::{
      file_name, BLOCKTREE_VERSION, OUTPUT_FLAG_HAS_TERMS, OUTPUT_FLAG_IS_FLOOR, POSTINGS_VERSION,
      SEGMENT_SUFFIX, TERMS_CODEC, TIM_CODEC, TIP_CODEC, TMD_CODEC,
  };
  use crate::postings_ll::{read_msb_vlong, BLOCK_SIZE};

  /// IntBlockTermState (Lucene912PostingsFormat.java:425-491): a term's
  /// postings entry points, decoded from the .tim metadata blob.
  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  pub struct TermState {
      pub doc_start_fp: u64,
      pub pos_start_fp: u64,
      pub last_pos_block_offset: i64,
      pub singleton_doc_id: i64,
  }

  /// A found term: stats + postings entry.
  #[derive(Clone, Copy, Debug)]
  pub struct TermEntry {
      pub doc_freq: u32,
      pub total_term_freq: u64,
      pub state: TermState,
  }

  /// Per-field record from .tmd (Lucene90BlockTreeTermsReader constructor
  /// :180-242).
  pub struct FieldTermsMeta {
      pub field_number: i32,
      pub num_terms: u64,
      pub root_code: Vec<u8>,
      pub sum_total_term_freq: u64,
      pub sum_doc_freq: u64,
      pub doc_count: i32,
      pub min_term: Vec<u8>,
      pub max_term: Vec<u8>,
      pub index_start_fp: u64,
      fst_metadata: FstMetadata,
  }

  /// Block-tree terms dictionary of one segment (.tim + .tip + .tmd).
  /// Field FSTs load lazily on first seek (spec §3 惰性加载).
  pub struct TermsDict {
      tim_in: IndexInput,
      tip_in: IndexInput,
      fields: Vec<FieldTermsMeta>,
      fsts: Vec<Option<FstReader>>,
  }

  /// Lucene90BlockTreeTermsReader.readBytesRef (:271-282).
  fn read_bytes_ref(input: &mut impl DataInput) -> io::Result<Vec<u8>> {
      let len = input.read_vint()? as usize;
      let mut b = vec![0u8; len];
      input.read_bytes(&mut b)?;
      Ok(b)
  }

  /// BitUtil.zigZagDecode (BitUtil.java:299).
  fn zigzag_decode(v: u64) -> i64 {
      ((v >> 1) as i64) ^ -((v & 1) as i64)
  }

  fn field_has_positions(field: &FieldInfo) -> bool {
      matches!(
          field.index_options,
          IndexOptions::DocsAndFreqsAndPositions
              | IndexOptions::DocsAndFreqsAndPositionsAndOffsets
      )
  }

  impl TermsDict {
      /// Opens .tmd/.tip/.tim and parses every field record
      /// (Lucene90BlockTreeTermsReader.<init> :126-269). The postings header
      /// lives inside .tmd (Lucene912PostingsReader.init :188-206).
      pub fn open(
          dir: &FSDirectory,
          segment: &str,
          segment_id: &[u8; 16],
          field_infos: &FieldInfos,
      ) -> io::Result<TermsDict> {
          // .tmd: field records + lengths + footer (streaming CRC).
          let mut tmd = dir.open_checksum_input(&file_name(segment, "tmd"))?;
          check_index_header(
              &mut tmd,
              TMD_CODEC,
              BLOCKTREE_VERSION,
              BLOCKTREE_VERSION,
              segment_id,
              SEGMENT_SUFFIX,
          )?;
          check_index_header(
              &mut tmd,
              TERMS_CODEC,
              POSTINGS_VERSION,
              POSTINGS_VERSION,
              segment_id,
              SEGMENT_SUFFIX,
          )?;
          let block_size = tmd.read_vint()?;
          if block_size != BLOCK_SIZE as i32 {
              return Err(corrupt(format!(
                  "expected postings blockSize {BLOCK_SIZE}, found {block_size}"
              )));
          }
          let num_fields = tmd.read_vint()?;
          if num_fields < 0 {
              return Err(corrupt(format!("invalid numFields {num_fields}")));
          }
          let mut fields = Vec::with_capacity(num_fields as usize);
          for _ in 0..num_fields {
              let field_number = tmd.read_vint()?;
              let num_terms = tmd.read_vlong()? as u64;
              if num_terms == 0 {
                  return Err(corrupt(format!(
                      "illegal numTerms 0 for field number {field_number}"
                  )));
              }
              let root_code = read_bytes_ref(&mut tmd)?;
              let field_info = field_infos.by_number(field_number).ok_or_else(|| {
                  corrupt(format!("invalid field number {field_number}"))
              })?;
              let sum_total_term_freq = tmd.read_vlong()? as u64;
              // :195-198 — DOCS fields store a single value
              // (sumDocFreq == sumTotalTermFreq).
              let sum_doc_freq = if field_info.index_options == IndexOptions::Docs {
                  sum_total_term_freq
              } else {
                  tmd.read_vlong()? as u64
              };
              let doc_count = tmd.read_vint()?;
              let min_term = read_bytes_ref(&mut tmd)?;
              let max_term = read_bytes_ref(&mut tmd)?;
              let index_start_fp = tmd.read_vlong()? as u64;
              let fst_metadata = FstMetadata::read(&mut tmd)?;
              fields.push(FieldTermsMeta {
                  field_number,
                  num_terms,
                  root_code,
                  sum_total_term_freq,
                  sum_doc_freq,
                  doc_count,
                  min_term,
                  max_term,
                  index_start_fp,
                  fst_metadata,
              });
          }
          let index_length = tmd.read_long()? as u64; // :243 (.tip)
          let terms_length = tmd.read_long()? as u64; // :244 (.tim)
          check_footer(&mut tmd)?; // :249

          let mut tip_in = dir.open_input(&file_name(segment, "tip"))?;
          check_index_header(
              &mut tip_in,
              TIP_CODEC,
              BLOCKTREE_VERSION,
              BLOCKTREE_VERSION,
              segment_id,
              SEGMENT_SUFFIX,
          )?;
          // retrieveChecksum (:328-336): length + trailing footer structure
          check_footer_structure(&tip_in, index_length)?;
          let mut tim_in = dir.open_input(&file_name(segment, "tim"))?;
          check_index_header(
              &mut tim_in,
              TIM_CODEC,
              BLOCKTREE_VERSION,
              BLOCKTREE_VERSION,
              segment_id,
              SEGMENT_SUFFIX,
          )?;
          check_footer_structure(&tim_in, terms_length)?;
          let fsts = fields.iter().map(|_| None).collect();
          Ok(TermsDict {
              tim_in,
              tip_in,
              fields,
              fsts,
          })
      }

      pub fn field_meta(&self, field_number: i32) -> Option<&FieldTermsMeta> {
          self.fields.iter().find(|f| f.field_number == field_number)
      }

      /// Loads the field's FST from .tip on first use
      /// (OffHeapFSTStore :39-61 loads the [indexStartFP, +numBytes) image).
      fn fst(&mut self, field_index: usize) -> io::Result<&FstReader> {
          if self.fsts[field_index].is_none() {
              let (index_start_fp, fst_metadata) = {
                  let m = &self.fields[field_index];
                  (m.index_start_fp, m.fst_metadata.clone())
              };
              let mut bytes = vec![0u8; fst_metadata.num_bytes as usize];
              self.tip_in.seek(index_start_fp)?;
              self.tip_in.read_bytes(&mut bytes)?;
              self.fsts[field_index] = Some(FstReader::new(bytes, &fst_metadata));
          }
          Ok(self.fsts[field_index].as_ref().unwrap())
      }

      /// SegmentTermsEnum.seekExact (:311-578), fresh-descent-only
      /// simplification: min/max pruning (:317-319), FST descent collecting
      /// candidate frames (:477-545), floor navigation
      /// (SegmentTermsEnumFrame.scanToFloorFrame :361-431), block load
      /// (:145-240) and linear entry scan (:547-660,:732-830).
      pub fn seek_exact(
          &mut self,
          field: &FieldInfo,
          term: &[u8],
      ) -> io::Result<Option<TermEntry>> {
          let Some(field_index) = self
              .fields
              .iter()
              .position(|f| f.field_number == field.number)
          else {
              return Ok(None); // field without terms: no .tmd record
          };
          {
              let meta = &self.fields[field_index];
              if term < meta.min_term.as_slice() || term > meta.max_term.as_slice() {
                  return Ok(None);
              }
          }
          // FST descent: (depth, output) candidates, deepest last.
          let mut frames: Vec<(usize, Vec<u8>)> =
              vec![(0, self.fields[field_index].root_code.clone())];
          {
              let traced = self.fst(field_index)?.trace_path(term)?;
              frames.extend(traced);
          }
          let (depth, output) = frames.last().unwrap();
          let depth = *depth;
          // pushFrame (:245-259): fp + flags from the output's leading
          // MSB-VLong; OUTPUT_FLAGS_NUM_BITS = 2 (Reader :72).
          let mut out_in = IndexInput::in_memory(output.clone());
          let code = read_msb_vlong(&mut out_in)?;
          let mut fp = code >> 2;
          let is_floor = code & OUTPUT_FLAG_IS_FLOOR != 0;
          let _has_terms = code & OUTPUT_FLAG_HAS_TERMS != 0;
          // scanToFloorFrame (Frame :361-431): pick the last floor sub-block
          // whose lead label <= the target byte at the frame's prefix length.
          if is_floor && depth < term.len() {
              let target_label = term[depth];
              let num_follow = out_in.read_vint()? as u32;
              let mut next_label = out_in.read_byte()?;
              if target_label >= next_label {
                  let fp_orig = fp;
                  for i in 0..num_follow {
                      let sub_code = out_in.read_vlong()? as u64;
                      fp = fp_orig + (sub_code >> 1);
                      if i + 1 == num_follow {
                          break;
                      }
                      next_label = out_in.read_byte()?;
                      if target_label < next_label {
                          break;
                      }
                  }
              }
          }
          self.scan_block(fp, depth, term, field)
      }

      /// loadBlock (SegmentTermsEnumFrame :145-240) + scanToTermLeaf/NonLeaf
      /// (:547-660,:732-830) for exactOnly=true: linear entry scan with
      /// incremental stats/meta decode (:433-481 + decodeTerm :235-277).
      /// Sub-block descent during scan is unnecessary for exact seek — every
      /// sub-block entry is itself an FST input (Lucene90BlockTreeTermsWriter
      /// .compileIndex :490-578), so the FST descent already landed on the
      /// deepest candidate block.
      fn scan_block(
          &mut self,
          fp: u64,
          prefix_len: usize,
          term: &[u8],
          field: &FieldInfo,
      ) -> io::Result<Option<TermEntry>> {
          let has_freqs = field.index_options != IndexOptions::Docs;
          let has_positions = field_has_positions(field);

          self.tim_in.seek(fp)?;
          let code = self.tim_in.read_vint()?;
          let ent_count = (code >> 1) as usize;
          let code_l = self.tim_in.read_vlong()? as u64;
          let is_leaf = code_l & 0x04 != 0;
          let num_suffix_bytes = (code_l >> 3) as usize;
          let compression = code_l & 0x03;
          if compression != 0 {
              return Err(corrupt(format!(
                  "unsupported suffix compression {compression} (writer emits NO_COMPRESSION)"
              )));
          }
          let mut suffix_bytes = vec![0u8; num_suffix_bytes];
          self.tim_in.read_bytes(&mut suffix_bytes)?;
          // suffix lengths blob, with the all-equal-bytes trick (writer :1026-1037)
          let mut num_sl_bytes = self.tim_in.read_vint()? as usize;
          let all_equal = num_sl_bytes & 1 != 0;
          num_sl_bytes >>= 1;
          let mut sl_bytes = vec![0u8; num_sl_bytes];
          if all_equal {
              let b = self.tim_in.read_byte()?;
              sl_bytes.fill(b);
          } else {
              self.tim_in.read_bytes(&mut sl_bytes)?;
          }
          let num_stat_bytes = self.tim_in.read_vint()? as usize;
          let mut stat_bytes = vec![0u8; num_stat_bytes];
          self.tim_in.read_bytes(&mut stat_bytes)?;
          let num_meta_bytes = self.tim_in.read_vint()? as usize;
          let mut meta_bytes = vec![0u8; num_meta_bytes];
          self.tim_in.read_bytes(&mut meta_bytes)?;

          let mut suffix_lengths = IndexInput::in_memory(sl_bytes);
          let mut stats = IndexInput::in_memory(stat_bytes);
          let mut meta = IndexInput::in_memory(meta_bytes);
          let mut suffix_pos = 0usize;
          let mut singleton_run: u32 = 0;
          // EMPTY_STATE (Lucene912PostingsWriter.java:425-457): fps 0, singleton -1
          let mut last_state = TermState {
              doc_start_fp: 0,
              pos_start_fp: 0,
              last_pos_block_offset: -1,
              singleton_doc_id: -1,
          };
          let mut last_entry: Option<TermEntry> = None;

          for _ in 0..ent_count {
              let (suffix_len, is_sub_block) = if is_leaf {
                  (suffix_lengths.read_vint()? as usize, false) // nextLeaf :300-312
              } else {
                  let c = suffix_lengths.read_vint()?; // nextNonLeaf :333-339
                  ((c >> 1) as usize, c & 1 != 0)
              };
              let suffix = &suffix_bytes[suffix_pos..suffix_pos + suffix_len];
              suffix_pos += suffix_len;
              if is_sub_block {
                  // back-pointer lives in the suffixLengths stream (:348-349);
                  // never followed for exact seek (see fn doc).
                  let _sub_fp = fp - suffix_lengths.read_vlong()? as u64;
              } else {
                  // stats (decodeMetaData :433-481)
                  let (doc_freq, total_term_freq) = if singleton_run > 0 {
                      singleton_run -= 1;
                      (1u32, 1u64)
                  } else {
                      let token = stats.read_vint()?;
                      if token & 1 != 0 {
                          singleton_run = (token >> 1) as u32;
                          (1u32, 1u64)
                      } else {
                          let df = (token >> 1) as u32;
                          let ttf = if has_freqs {
                              df as u64 + stats.read_vlong()? as u64
                          } else {
                              df as u64
                          };
                          (df, ttf)
                      }
                  };
                  // metadata (Lucene912PostingsReader.decodeTerm :235-277)
                  let l = meta.read_vlong()? as u64;
                  if l & 1 == 0 {
                      last_state.doc_start_fp += l >> 1;
                      last_state.singleton_doc_id = if doc_freq == 1 {
                          meta.read_vint()? as i64
                      } else {
                          -1
                      };
                  } else {
                      let delta = zigzag_decode(l >> 1);
                      last_state.singleton_doc_id += delta;
                  }
                  if has_positions {
                      last_state.pos_start_fp += meta.read_vlong()? as u64;
                      last_state.last_pos_block_offset =
                          if total_term_freq > BLOCK_SIZE as u64 {
                              meta.read_vlong()?
                          } else {
                              -1
                          };
                  }
                  last_entry = Some(TermEntry {
                      doc_freq,
                      total_term_freq,
                      state: last_state,
                  });
              }
              match suffix.cmp(&term[prefix_len..]) {
                  Ordering::Less => continue,
                  Ordering::Greater => return Ok(None),
                  Ordering::Equal => {
                      if is_sub_block {
                          // assert termExists in Frame :790-793 — the FST
                          // descent should have consumed this prefix.
                          return Err(corrupt(
                              "block-tree scan hit an exact sub-block match",
                          ));
                      }
                      return Ok(last_entry);
                  }
              }
          }
          Ok(None) // SeekStatus.END → NOT_FOUND for exact seek
      }
  }
  ```

  `crates/codec-lucene9/src/lib.rs`：模块区加：

  ```rust
  pub mod terms_read;
  ```

  并在 re-export 区加：

  ```rust
  pub use terms_read::{TermEntry, TermState, TermsDict};
  ```

- [ ] **Step 6.4: 跑测试确认通过**

  ```
  $ cargo test -p codec-lucene9 terms_read:: 2>&1 | tail -3
  test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  $ cargo test -p codec-lucene9 2>&1 | tail -3   # 全量回归
  test result: ok. 119 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  ```

- [ ] **Step 6.5: 提交**

  ```
  git add crates/codec-lucene9/src/terms_read.rs crates/codec-lucene9/src/lib.rs crates/codec-lucene9/src/postings.rs
  git commit -m "feat: block-tree terms dict reader (.tmd/.tip/.tim, seek_exact)"
  ```

---

## Task 7: postings 枚举（postings_read.rs 新增）

**Files:**
- Create: `crates/codec-lucene9/src/postings_read.rs`
- Modify: `crates/codec-lucene9/src/lib.rs`（`pub mod postings_read;` + re-export）
- Test: `crates/codec-lucene9/src/postings_read.rs` 的 `#[cfg(test)]` 模块

**Interfaces:**
- Consumes: T1 `DataInput` / `IndexInput` / `codec_util::{check_index_header, check_footer, check_footer_structure}`；T2 `for_delta_util_decode` / `pfor_util_decode` / `read_group_vints` / `BLOCK_SIZE`；T6 `TermEntry` / `TermState`；写侧 `PostingsWriter`（测试语料）。
- Produces:
  ```rust
  pub const NO_MORE_DOCS: i32 = i32::MAX;  // DocIdSetIterator.NO_MORE_DOCS
  pub struct PostingsReader { .. }
  impl PostingsReader {
      pub fn open(dir: &FSDirectory, segment: &str, segment_id: &[u8; 16]) -> io::Result<PostingsReader>;
      pub fn docs(&self, entry: &TermEntry) -> io::Result<DocsEnum>;
      pub fn docs_and_freqs(&self, entry: &TermEntry) -> io::Result<DocsFreqsEnum>;
  }
  pub struct DocsEnum { .. }
  impl DocsEnum {
      pub fn doc_id(&self) -> i32;
      pub fn next_doc(&mut self) -> io::Result<i32>;
      pub fn advance(&mut self, target: i32) -> io::Result<i32>;
  }
  pub struct DocsFreqsEnum { .. }
  impl DocsFreqsEnum {
      pub fn doc_id(&self) -> i32;
      pub fn next_doc(&mut self) -> io::Result<i32>;
      pub fn advance(&mut self, target: i32) -> io::Result<i32>;
      pub fn freq(&self) -> u32;
  }
  ```

  关键语义（已对照 9.12.3 核实）：df==1 singleton 不读 .doc（docID 取 `singleton_doc_id`）；满 128 块 = level-0 skip 条目整体跳过 + `for_delta_util_decode` + 前缀和（+ hasFreqs 时 `pfor_util_decode`）；尾部 = group-vints（hasFreqs 时 `(delta<<1)|freqIs1`，freq≠1 的另存 VInt）；跨过 4096 文档边界时 inline 消费 level-1 记录（`skipLevel1To` :522-546 镜像）。`advance` = `next_doc` 循环（M1 线性，skip-data 驱动留待阶段 3 Boolean）。

### Steps

- [ ] **Step 7.1: 写失败测试** — `crates/codec-lucene9/src/postings_read.rs`（先建只有模块文档与测试的文件）：

  ```rust
  //! Postings (.doc) reader: Docs and DocsAndFreqs enumeration
  //! (Lucene912PostingsReader BlockDocsEnum, Lucene 9.12.3).

  #[cfg(test)]
  mod tests {
      use super::*;
      use crate::directory::FSDirectory;
      use crate::field_infos::{FieldInfo, FieldInfos, IndexOptions};
      use crate::postings::PostingsWriter;
      use crate::terms_read::{TermEntry, TermState};
      use std::fs;

      fn temp_dir(tag: &str) -> std::path::PathBuf {
          let dir = std::env::temp_dir().join(format!(
              "codec-lucene9-postings-{}-{}",
              tag,
              std::process::id()
          ));
          let _ = fs::remove_dir_all(&dir);
          dir
      }

      fn indexed(name: &str, number: i32, opts: IndexOptions) -> FieldInfo {
          FieldInfo {
              name: name.to_string(),
              number,
              omit_norms: true,
              index_options: opts,
              ..FieldInfo::stored(name, number)
          }
      }

      /// kw (DOCS): "big" df=200 dense docs; "tail" df=3
      /// tx (DOCS_AND_FREQS): "hot" df=5000 dense, freqs all 1 (level-1 跨界 + 常量块);
      ///                      "warm" df=200 docs step 3, freqs 含 PFor 异常值;
      ///                      "one" df=1 singleton doc 42 freq 7
      fn write_segment(dir: &FSDirectory) -> (FieldInfos, Vec<u32>, Vec<u32>) {
          let id = [4u8; 16];
          let kw = indexed("kw", 0, IndexOptions::Docs);
          let tx = indexed("tx", 1, IndexOptions::DocsAndFreqs);
          let mut w = PostingsWriter::new(dir, "_0", &id).unwrap();
          w.start_field(&kw, 6000).unwrap();
          let big: Vec<u32> = (0..200).collect();
          w.write_term(b"big", &big, &vec![1; 200], None).unwrap();
          w.write_term(b"tail", &[10, 20, 30], &[1, 1, 1], None).unwrap();
          w.finish_field().unwrap();
          w.start_field(&tx, 6000).unwrap();
          let hot: Vec<u32> = (0..5000).collect();
          w.write_term(b"hot", &hot, &vec![1; 5000], None).unwrap();
          let warm_docs: Vec<u32> = (0..200).map(|i| i * 3).collect();
          let mut warm_freqs: Vec<u32> = (0..200).map(|i| (i % 5) + 1).collect();
          warm_freqs[3] = 3000;
          warm_freqs[77] = 65535;
          warm_freqs[100] = 999;
          w.write_term(b"warm", &warm_docs, &warm_freqs, None).unwrap();
          w.write_term(b"one", &[42], &[7], None).unwrap();
          w.finish_field().unwrap();
          w.finish().unwrap();
          let fis = FieldInfos::new(vec![kw, tx]);
          fis.write(dir, "_0", &id, "").unwrap();
          (fis, warm_docs, warm_freqs)
      }

      /// 直接按 TermState 手工构造 TermEntry（不走 terms dict，隔离 T6）。
      fn entry(doc_freq: u32, ttf: u64, doc_start_fp: u64, singleton: i64) -> TermEntry {
          TermEntry {
              doc_freq,
              total_term_freq: ttf,
              state: TermState {
                  doc_start_fp,
                  pos_start_fp: 0,
                  last_pos_block_offset: -1,
                  singleton_doc_id: singleton,
              },
          }
      }

      /// 从 .tim 查 term 的 TermEntry（端到端走 T6 reader）。
      fn seek(dir: &FSDirectory, fis: &FieldInfos, field: &str, term: &[u8]) -> TermEntry {
          let mut dict = crate::terms_read::TermsDict::open(dir, "_0", &[4u8; 16], fis).unwrap();
          let fi = fis.by_name(field).unwrap();
          dict.seek_exact(fi, term).unwrap().expect("term must exist")
      }

      #[test]
      fn docs_enum_sequences() {
          let root = temp_dir("docs");
          let dir = FSDirectory::open(&root).unwrap();
          let (fis, _, _) = write_segment(&dir);
          let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();
          // dense 200 docs, 1 full block + tail 72
          let e = seek(&dir, &fis, "kw", b"big");
          let mut en = postings.docs(&e).unwrap();
          let mut got = Vec::new();
          loop {
              let d = en.next_doc().unwrap();
              if d == NO_MORE_DOCS {
                  break;
              }
              got.push(d);
          }
          assert_eq!(got, (0..200).collect::<Vec<i32>>());
          // tail-only
          let e = seek(&dir, &fis, "kw", b"tail");
          let mut en = postings.docs(&e).unwrap();
          assert_eq!(en.next_doc().unwrap(), 10);
          assert_eq!(en.next_doc().unwrap(), 20);
          assert_eq!(en.next_doc().unwrap(), 30);
          assert_eq!(en.next_doc().unwrap(), NO_MORE_DOCS);
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn docs_freqs_across_level1_boundary() {
          let root = temp_dir("hot");
          let dir = FSDirectory::open(&root).unwrap();
          let (fis, _, _) = write_segment(&dir);
          let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();
          // df=5000 dense: 39 full blocks (all-ones deltas) + tail 8, 跨过 4096 的 level-1 记录
          let e = seek(&dir, &fis, "tx", b"hot");
          let mut en = postings.docs_and_freqs(&e).unwrap();
          for expected in 0..5000 {
              assert_eq!(en.next_doc().unwrap(), expected);
              assert_eq!(en.freq(), 1);
          }
          assert_eq!(en.next_doc().unwrap(), NO_MORE_DOCS);
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn docs_freqs_with_pfor_exceptions_and_tail() {
          let root = temp_dir("warm");
          let dir = FSDirectory::open(&root).unwrap();
          let (fis, warm_docs, warm_freqs) = write_segment(&dir);
          let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();
          let e = seek(&dir, &fis, "tx", b"warm");
          let mut en = postings.docs_and_freqs(&e).unwrap();
          for i in 0..200 {
              assert_eq!(en.next_doc().unwrap(), warm_docs[i] as i32, "doc {i}");
              assert_eq!(en.freq(), warm_freqs[i], "freq {i}");
          }
          assert_eq!(en.next_doc().unwrap(), NO_MORE_DOCS);
          // singleton: 不读 .doc，freq = totalTermFreq
          let e = seek(&dir, &fis, "tx", b"one");
          assert_eq!(e.state.singleton_doc_id, 42);
          let mut en = postings.docs_and_freqs(&e).unwrap();
          assert_eq!(en.next_doc().unwrap(), 42);
          assert_eq!(en.freq(), 7);
          assert_eq!(en.next_doc().unwrap(), NO_MORE_DOCS);
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn singleton_termstate_entry_constructed_by_hand() {
          // df==1 时 .doc 中没有任何字节（写侧 write_term 对 df==1 不写 postings）
          let root = temp_dir("single");
          let dir = FSDirectory::open(&root).unwrap();
          write_segment(&dir);
          let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();
          let e = entry(1, 3, 0, 17);
          let mut en = postings.docs(&e).unwrap();
          assert_eq!(en.next_doc().unwrap(), 17);
          assert_eq!(en.next_doc().unwrap(), NO_MORE_DOCS);
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn advance_is_linear_but_correct() {
          let root = temp_dir("advance");
          let dir = FSDirectory::open(&root).unwrap();
          let (fis, _, _) = write_segment(&dir);
          let postings = PostingsReader::open(&dir, "_0", &[4u8; 16]).unwrap();
          let e = seek(&dir, &fis, "kw", b"big");
          let mut en = postings.docs(&e).unwrap();
          assert_eq!(en.advance(57).unwrap(), 57);
          assert_eq!(en.advance(57).unwrap(), 57); // 已在目标上不动
          assert_eq!(en.advance(199).unwrap(), 199);
          assert_eq!(en.advance(200).unwrap(), NO_MORE_DOCS);
          fs::remove_dir_all(&root).unwrap();
      }
  }
  ```

- [ ] **Step 7.2: 跑测试确认失败**

  ```
  $ cargo test -p codec-lucene9 postings_read:: 2>&1 | tail -5
  error[E0433]: failed to resolve: use of undeclared type `PostingsReader`
  ```

- [ ] **Step 7.3: 最小实现** — `crates/codec-lucene9/src/postings_read.rs` 在模块文档与测试模块之间插入：

  ```rust
  use std::io;

  use crate::codec_util::{check_footer, check_footer_structure, check_index_header};
  use crate::directory::FSDirectory;
  use crate::io::{DataInput, IndexInput};
  use crate::postings::{file_name, DOC_CODEC, POSTINGS_VERSION, PSM_CODEC, SEGMENT_SUFFIX};
  use crate::postings_ll::{for_delta_util_decode, pfor_util_decode, read_group_vints, BLOCK_SIZE};
  use crate::terms_read::TermEntry;

  /// DocIdSetIterator.NO_MORE_DOCS.
  pub const NO_MORE_DOCS: i32 = i32::MAX;

  /// Lucene912PostingsFormat.java:347-352.
  const LEVEL1_NUM_DOCS: u32 = 4096;

  /// Owns the segment's .doc stream (Lucene912PostingsReader :83-206).
  pub struct PostingsReader {
      doc_in: IndexInput,
  }

  impl PostingsReader {
      /// Opens .psm + .doc, validating headers and exact lengths
      /// (Lucene912PostingsReader constructor :83-185).
      pub fn open(dir: &FSDirectory, segment: &str, segment_id: &[u8; 16]) -> io::Result<PostingsReader> {
          let mut psm = dir.open_checksum_input(&file_name(segment, "psm"))?;
          check_index_header(
              &mut psm,
              PSM_CODEC,
              POSTINGS_VERSION,
              POSTINGS_VERSION,
              segment_id,
              SEGMENT_SUFFIX,
          )?;
          // max impact params (levels 0/1): needed only for impact-driven
          // skipping, which we never do — parsed and discarded (:100-103).
          let _ = psm.read_int()?;
          let _ = psm.read_int()?;
          let _ = psm.read_int()?;
          let _ = psm.read_int()?;
          let doc_len = psm.read_long()? as u64;
          // posLen is present iff the writer created a .pos file (:106).
          if dir.file_exists(&file_name(segment, "pos")) {
              let _pos_len = psm.read_long()?;
          }
          check_footer(&mut psm)?;
          let mut doc_in = dir.open_input(&file_name(segment, "doc"))?;
          check_index_header(
              &mut doc_in,
              DOC_CODEC,
              POSTINGS_VERSION,
              POSTINGS_VERSION,
              segment_id,
              SEGMENT_SUFFIX,
          )?;
          // retrieveChecksum (:150-172): length + trailing footer structure
          check_footer_structure(&doc_in, doc_len)?;
          Ok(PostingsReader { doc_in })
      }

      /// Docs iterator over a DOCS field's postings (no freq blocks on disk).
      pub fn docs(&self, entry: &TermEntry) -> io::Result<DocsEnum> {
          Ok(DocsEnum {
              core: EnumCore::new(self.fresh_input()?, entry, false)?,
          })
      }

      /// Docs+freqs iterator over a field with frequencies
      /// (IndexOptions >= DOCS_AND_FREQS).
      pub fn docs_and_freqs(&self, entry: &TermEntry) -> io::Result<DocsFreqsEnum> {
          Ok(DocsFreqsEnum {
              core: EnumCore::new(self.fresh_input()?, entry, true)?,
          })
      }

      /// An independent positioned stream over .doc (enums own their cursor).
      fn fresh_input(&self) -> io::Result<IndexInput> {
          self.doc_in.slice(0, self.doc_in.length())
      }
  }

  /// BlockDocsEnum state machine (:345-625), shared by the two public enums.
  /// `has_freqs` selects the freq-block decode (DocsFreqsEnum) — Java decodes
  /// freqs lazily on first `freq()` call; M1 decodes eagerly per block
  /// (identical output, simpler control flow).
  struct EnumCore {
      doc_in: IndexInput,
      doc_freq: u32,
      total_term_freq: u64,
      singleton_doc_id: i64,
      has_freqs: bool,
      doc: i64,
      prev_doc_id: i64,
      doc_count_upto: u32,
      level0_last_doc: i64,
      level1_last_doc: i64,
      level1_doc_end_fp: u64,
      level1_doc_count_upto: u32,
      doc_buffer: [u64; BLOCK_SIZE + 1],
      freq_buffer: [u32; BLOCK_SIZE],
      doc_buffer_upto: usize,
  }

  impl EnumCore {
      /// reset (:413-446).
      fn new(doc_in: IndexInput, entry: &TermEntry, has_freqs: bool) -> io::Result<EnumCore> {
          let mut c = EnumCore {
              doc_in,
              doc_freq: entry.doc_freq,
              total_term_freq: entry.total_term_freq,
              singleton_doc_id: entry.state.singleton_doc_id,
              has_freqs,
              doc: -1,
              prev_doc_id: -1,
              doc_count_upto: 0,
              level0_last_doc: -1,
              level1_last_doc: -1,
              level1_doc_end_fp: 0,
              level1_doc_count_upto: 0,
              doc_buffer: [0; BLOCK_SIZE + 1],
              freq_buffer: [0; BLOCK_SIZE],
              doc_buffer_upto: BLOCK_SIZE,
          };
          if entry.doc_freq < LEVEL1_NUM_DOCS {
              c.level1_last_doc = NO_MORE_DOCS as i64;
              if entry.doc_freq > 1 {
                  c.doc_in.seek(entry.state.doc_start_fp)?;
              }
          } else {
              c.level1_last_doc = -1;
              c.level1_doc_end_fp = entry.state.doc_start_fp;
          }
          Ok(c)
      }

      /// nextDoc (:589-596).
      fn next_doc(&mut self) -> io::Result<i32> {
          if self.doc == self.level0_last_doc {
              self.move_to_next_level0_block()?;
          }
          self.doc = self.doc_buffer[self.doc_buffer_upto] as i64;
          self.doc_buffer_upto += 1;
          Ok(self.doc as i32)
      }

      /// moveToNextLevel0Block (:573-587).
      fn move_to_next_level0_block(&mut self) -> io::Result<()> {
          if self.doc == self.level1_last_doc {
              self.skip_level1_to(self.doc + 1)?;
          }
          self.prev_doc_id = self.level0_last_doc;
          if self.doc_freq - self.doc_count_upto >= BLOCK_SIZE as u32 {
              let skip0_num_bytes = self.doc_in.read_vlong()? as u64;
              self.doc_in.skip_bytes(skip0_num_bytes)?;
              self.refill_full_block()?;
              self.level0_last_doc = self.doc_buffer[BLOCK_SIZE - 1] as i64;
          } else {
              self.level0_last_doc = NO_MORE_DOCS as i64;
              self.refill_remainder()?;
          }
          Ok(())
      }

      /// skipLevel1To (:522-546): parses the level-1 record inline (VInt
      /// docDelta, hasFreqs 时 VLong level1TotalBytes + Short numSkipBytes),
      /// skipping impacts/pos sections without interpreting them.
      fn skip_level1_to(&mut self, target: i64) -> io::Result<()> {
          loop {
              self.prev_doc_id = self.level1_last_doc;
              self.level0_last_doc = self.level1_last_doc;
              self.doc_in.seek(self.level1_doc_end_fp)?;
              self.doc_count_upto = self.level1_doc_count_upto;
              self.level1_doc_count_upto += LEVEL1_NUM_DOCS;
              if self.doc_freq - self.doc_count_upto < LEVEL1_NUM_DOCS {
                  self.level1_last_doc = NO_MORE_DOCS as i64;
                  break;
              }
              self.level1_last_doc += self.doc_in.read_vint()? as i64;
              self.level1_doc_end_fp =
                  self.doc_in.read_vlong()? as u64 + self.doc_in.file_pointer();
              if self.level1_last_doc >= target {
                  if self.has_freqs {
                      let num_skip_bytes = self.doc_in.read_short()? as u16 as u64;
                      self.doc_in.skip_bytes(num_skip_bytes)?;
                  }
                  break;
              }
          }
          Ok(())
      }

      /// refillFullBlock (:484-499): ForDelta decode + prefix sum; freq block
      /// decoded eagerly (Java defers it to the first freq() call via freqFP).
      fn refill_full_block(&mut self) -> io::Result<()> {
          let mut deltas = [0u64; BLOCK_SIZE];
          for_delta_util_decode(&mut self.doc_in, &mut deltas)?;
          prefix_sum(&mut deltas, self.prev_doc_id);
          self.doc_buffer[..BLOCK_SIZE].copy_from_slice(&deltas);
          if self.has_freqs {
              let mut freqs = [0u64; BLOCK_SIZE];
              pfor_util_decode(&mut self.doc_in, &mut freqs)?;
              for (dst, src) in self.freq_buffer.iter_mut().zip(freqs) {
                  *dst = src as u32;
              }
          }
          self.doc_count_upto += BLOCK_SIZE as u32;
          self.prev_doc_id = self.doc_buffer[BLOCK_SIZE - 1] as i64;
          self.doc_buffer_upto = 0;
          Ok(())
      }

      /// refillRemainder (:501-520) + PostingsUtil.readVIntBlock (:30-52):
      /// singleton (no file bytes at all), or a group-vint tail with
      /// freq==1 folded into the delta's low bit.
      fn refill_remainder(&mut self) -> io::Result<()> {
          let left = (self.doc_freq - self.doc_count_upto) as usize;
          if self.doc_freq == 1 {
              self.doc_buffer[0] = self.singleton_doc_id as u64;
              self.freq_buffer[0] = self.total_term_freq as u32;
              self.doc_buffer[1] = NO_MORE_DOCS as u64;
              self.doc_count_upto += 1;
          } else {
              let mut values = [0u32; BLOCK_SIZE];
              read_group_vints(&mut self.doc_in, &mut values[..left])?;
              if self.has_freqs {
                  for i in 0..left {
                      let freq_is_one = values[i] & 1;
                      self.doc_buffer[i] = (values[i] >> 1) as u64;
                      self.freq_buffer[i] = if freq_is_one == 1 {
                          1
                      } else {
                          self.doc_in.read_vint()? as u32
                      };
                  }
              } else {
                  for i in 0..left {
                      self.doc_buffer[i] = values[i] as u64;
                  }
              }
              prefix_sum(&mut self.doc_buffer[..left], self.prev_doc_id);
              self.doc_buffer[left] = NO_MORE_DOCS as u64;
              self.doc_count_upto += left as u32;
          }
          self.doc_buffer_upto = 0;
          Ok(())
      }

      fn freq(&self) -> u32 {
          if self.has_freqs {
              self.freq_buffer[self.doc_buffer_upto - 1]
          } else {
              1
          }
      }
  }

  /// Lucene912PostingsReader.prefixSum (:208-213): buffer[0] += base, then
  /// running sum (matches ForDeltaUtil.decodeAndPrefixSum's net result
  /// :276-283 — the SIMD-structured prefixSum8/16/32 are math-equivalent).
  fn prefix_sum(buffer: &mut [u64], base: i64) {
      if buffer.is_empty() {
          return;
      }
      buffer[0] = buffer[0].wrapping_add(base as u64);
      for i in 1..buffer.len() {
          buffer[i] = buffer[i].wrapping_add(buffer[i - 1]);
      }
  }

  /// M1: linear advance (next_doc loop). Skip-data-driven advance arrives
  /// with Boolean conjunction (search spec phase 3).
  fn advance_linear(core: &mut EnumCore, target: i32) -> io::Result<i32> {
      if core.doc >= target as i64 {
          return Ok(core.doc as i32);
      }
      loop {
          let d = core.next_doc()?;
          if d >= target {
              return Ok(d);
          }
      }
  }

  /// Docs iterator (no frequencies; for IndexOptions.DOCS fields).
  pub struct DocsEnum {
      core: EnumCore,
  }

  impl DocsEnum {
      pub fn doc_id(&self) -> i32 {
          self.core.doc as i32
      }

      pub fn next_doc(&mut self) -> io::Result<i32> {
          self.core.next_doc()
      }

      pub fn advance(&mut self, target: i32) -> io::Result<i32> {
          advance_linear(&mut self.core, target)
      }
  }

  /// Docs + freqs iterator (IndexOptions >= DOCS_AND_FREQS fields).
  pub struct DocsFreqsEnum {
      core: EnumCore,
  }

  impl DocsFreqsEnum {
      pub fn doc_id(&self) -> i32 {
          self.core.doc as i32
      }

      pub fn next_doc(&mut self) -> io::Result<i32> {
          self.core.next_doc()
      }

      pub fn advance(&mut self, target: i32) -> io::Result<i32> {
          advance_linear(&mut self.core, target)
      }

      /// PostingsEnum.freq(): current doc's term frequency.
      pub fn freq(&self) -> u32 {
          self.core.freq()
      }
  }
  ```

  `crates/codec-lucene9/src/lib.rs` 加：

  ```rust
  pub mod postings_read;
  ```

  与 re-export：

  ```rust
  pub use postings_read::{DocsEnum, DocsFreqsEnum, PostingsReader, NO_MORE_DOCS};
  ```

- [ ] **Step 7.4: 跑测试确认通过**

  ```
  $ cargo test -p codec-lucene9 postings_read:: 2>&1 | tail -3
  test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  $ cargo test -p codec-lucene9 2>&1 | tail -3   # 全量回归
  test result: ok. 124 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  ```

- [ ] **Step 7.5: 提交**

  ```
  git add crates/codec-lucene9/src/postings_read.rs crates/codec-lucene9/src/lib.rs
  git commit -m "feat: postings reader (.doc Docs/DocsFreqs enums, singleton + PFOR + level-1 framing)"
  ```

---

## Task 8: core search 模块（SegmentReader / Reader / Query / DocIter / collector / Searcher）

**Files:**
- Create: `crates/core/src/search/mod.rs`
- Create: `crates/core/src/search/segment_reader.rs`
- Create: `crates/core/src/search/reader.rs`
- Create: `crates/core/src/search/query.rs`
- Create: `crates/core/src/search/doc_iter.rs`
- Create: `crates/core/src/search/collector.rs`
- Create: `crates/core/src/search/searcher.rs`
- Modify: `crates/core/src/lib.rs`（`pub mod search;` + 更新模块文档头）
- Test: `crates/core/src/search/mod.rs` 的 `#[cfg(test)]` 模块（端到端语义测试）

**Interfaces:**
- Consumes: codec-lucene9 的 `FSDirectory` / `SegmentInfos::read_latest` / `SegmentInfo` / `FieldInfos` / `TermsDict` / `PostingsReader` / `DocsEnum` / `DocsFreqsEnum` / `NO_MORE_DOCS`（T1–T7 产物）；core 现有 `IndexWriter` / `IndexWriterConfig` / `Document` / `FieldValue` / `Schema` / `FieldSpec`（测试语料）。
- Produces:
  ```rust
  // query.rs
  #[derive(Clone, Debug, PartialEq, Eq)]
  pub enum Query {
      Term { field: String, term: Vec<u8> },
      MatchAll,
  }
  impl Query {
      pub fn term(field: &str, term: &str) -> Query;
  }
  // doc_iter.rs
  pub trait DocIter {
      fn doc_id(&self) -> i32;
      fn next_doc(&mut self) -> io::Result<i32>;
      fn advance(&mut self, target: i32) -> io::Result<i32>; // default: next_doc 循环
      fn freq(&self) -> u32;                                 // default: 1
  }
  pub struct MatchAllIter { .. }
  pub enum SegmentDocIter { Docs(DocsEnum), Freqs(DocsFreqsEnum), All(MatchAllIter) }
  // collector.rs
  pub trait Collector { fn collect(&mut self, doc: i32, freq: u32); }
  pub struct CountCollector { pub count: u64 }
  pub struct TopDocCollector { pub total: u64, pub docs: Vec<i32>, .. }
  impl TopDocCollector { pub fn new(top_n: usize) -> Self; }
  pub struct FreqSumCollector { pub total_freq: u64 }
  // segment_reader.rs / reader.rs / searcher.rs
  pub struct SegmentReader { .. }
  pub struct Reader { .. }
  impl Reader {
      pub fn open(dir: &FSDirectory) -> io::Result<Reader>;
      pub fn max_doc(&self) -> i32;
      pub fn segment_count(&self) -> usize;
  }
  pub struct Searcher { .. }
  impl Searcher {
      pub fn open(dir: &FSDirectory) -> io::Result<Searcher>;
      pub fn max_doc(&self) -> i32;
      pub fn segment_count(&self) -> usize;
      pub fn search<C: Collector>(&mut self, query: &Query, collector: &mut C) -> io::Result<()>;
      pub fn count(&mut self, query: &Query) -> io::Result<u64>;
      pub fn top_docs(&mut self, query: &Query, n: usize) -> io::Result<(u64, Vec<i32>)>;
      pub fn freq_sum(&mut self, query: &Query) -> io::Result<u64>;
  }
  ```

  语义决定（与 Java 对齐）：未知字段 / 无 postings 字段的 Term 查询返回空结果（Java `TermQuery` 行为：无 terms 即无命中），不报错；全局 docID = docBase（segments_N 中段序）+ 段内 docID；`TopDocCollector` 等价 `Sort.INDEXORDER` 的 topN = 按 docID 升序的前 N 个命中。

### Steps

- [ ] **Step 8.1: 写失败测试** — `crates/core/src/search/mod.rs`（先建只含模块声明与测试的文件）：

  ```rust
  //! Search read path (search spec §3): per-segment iteration, docID-ordered
  //! and count collectors, Term and MatchAll queries (ConstantScore semantics).

  pub mod collector;
  pub mod doc_iter;
  pub mod query;
  pub mod reader;
  pub mod searcher;
  pub mod segment_reader;

  pub use collector::{Collector, CountCollector, FreqSumCollector, TopDocCollector};
  pub use doc_iter::{DocIter, MatchAllIter, SegmentDocIter};
  pub use query::Query;
  pub use reader::Reader;
  pub use searcher::Searcher;
  pub use segment_reader::SegmentReader;

  #[cfg(test)]
  mod tests {
      use super::*;
      use crate::{Document, FieldSpec, FieldValue, IndexWriter, IndexWriterConfig, Schema};
      use codec_lucene9::FSDirectory;
      use std::fs;
      use std::path::PathBuf;

      fn temp_dir(tag: &str) -> PathBuf {
          let dir = std::env::temp_dir().join(format!(
              "rustlucene-search-{}-{}",
              tag,
              std::process::id()
          ));
          let _ = fs::remove_dir_all(&dir);
          dir
      }

      fn schema() -> Schema {
          let mut s = Schema::new();
          s.add(FieldSpec::keyword("level"));
          s.add(FieldSpec::keyword("tid"));
          s.add(FieldSpec::text("message"));
          s.add(FieldSpec::stored("title"));
          s
      }

      fn doc(level: &str, tid: &str, message: &str) -> Document {
          let mut d = Document::new();
          d.add("level", FieldValue::Keyword(level.to_string()));
          d.add("tid", FieldValue::Keyword(tid.to_string()));
          d.add("message", FieldValue::Text(message.to_string()));
          d.add("title", FieldValue::Text("stored only".to_string()));
          d
      }

      #[test]
      fn term_and_matchall_single_segment() {
          let root = temp_dir("single");
          let mut w = IndexWriter::create(&root, schema(), IndexWriterConfig::default()).unwrap();
          for i in 0..10 {
              let level = if i % 2 == 0 { "INFO" } else { "WARN" };
              w.add_document(doc(level, &format!("tid-{i}"), &format!("w{} common", i % 3)))
                  .unwrap();
          }
          w.commit().unwrap();
          drop(w);

          let dir = FSDirectory::open(&root).unwrap();
          let mut s = Searcher::open(&dir).unwrap();
          assert_eq!(s.max_doc(), 10);
          assert_eq!(s.segment_count(), 1);
          // term count（keyword, DOCS）
          assert_eq!(s.count(&Query::term("level", "INFO")).unwrap(), 5);
          assert_eq!(s.count(&Query::term("level", "WARN")).unwrap(), 5);
          assert_eq!(s.count(&Query::term("level", "DEBUG")).unwrap(), 0);
          // singleton
          assert_eq!(s.count(&Query::term("tid", "tid-7")).unwrap(), 1);
          let (total, docs) = s.top_docs(&Query::term("tid", "tid-7"), 10).unwrap();
          assert_eq!(total, 1);
          assert_eq!(docs, vec![7]);
          // text 字段（DOCS_AND_FREQS）count + topN + freqsum
          assert_eq!(s.count(&Query::term("message", "common")).unwrap(), 10);
          let (total, docs) = s.top_docs(&Query::term("message", "common"), 4).unwrap();
          assert_eq!(total, 10);
          assert_eq!(docs, vec![0, 1, 2, 3]);
          assert_eq!(s.freq_sum(&Query::term("message", "common")).unwrap(), 10);
          // matchall
          assert_eq!(s.count(&Query::MatchAll).unwrap(), 10);
          let (_, docs) = s.top_docs(&Query::MatchAll, 20).unwrap();
          assert_eq!(docs, (0..10).collect::<Vec<i32>>());
          // 未知字段 / stored-only 字段 → 空（Java TermQuery 语义）
          assert_eq!(s.count(&Query::term("nope", "x")).unwrap(), 0);
          assert_eq!(s.count(&Query::term("title", "stored")).unwrap(), 0);
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn multi_segment_docbase_mapping() {
          let root = temp_dir("multiseg");
          let mut w = IndexWriter::create(&root, schema(), IndexWriterConfig::default()).unwrap();
          for i in 0..3 {
              w.add_document(doc("INFO", &format!("tid-{i}"), "alpha")).unwrap();
          }
          w.commit().unwrap();
          for i in 3..7 {
              w.add_document(doc("WARN", &format!("tid-{i}"), "alpha")).unwrap();
          }
          w.commit().unwrap();
          drop(w);

          let dir = FSDirectory::open(&root).unwrap();
          let mut s = Searcher::open(&dir).unwrap();
          assert_eq!(s.segment_count(), 2);
          assert_eq!(s.max_doc(), 7);
          // 跨段 term 查询
          assert_eq!(s.count(&Query::term("level", "INFO")).unwrap(), 3);
          assert_eq!(s.count(&Query::term("level", "WARN")).unwrap(), 4);
          let (total, docs) = s.top_docs(&Query::term("message", "alpha"), 20).unwrap();
          assert_eq!(total, 7);
          assert_eq!(docs, vec![0, 1, 2, 3, 4, 5, 6]);
          let (_, docs) = s.top_docs(&Query::MatchAll, 20).unwrap();
          assert_eq!(docs, vec![0, 1, 2, 3, 4, 5, 6]);
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn high_df_crosses_level1_boundary() {
          let root = temp_dir("bigdf");
          let mut w = IndexWriter::create(&root, schema(), IndexWriterConfig::default()).unwrap();
          for i in 0..5000 {
              w.add_document(doc("INFO", &format!("tid-{i}"), "alpha")).unwrap();
          }
          w.commit().unwrap();
          drop(w);
          let dir = FSDirectory::open(&root).unwrap();
          let mut s = Searcher::open(&dir).unwrap();
          assert_eq!(s.count(&Query::term("level", "INFO")).unwrap(), 5000);
          let (total, docs) = s.top_docs(&Query::term("level", "INFO"), 3).unwrap();
          assert_eq!(total, 5000);
          assert_eq!(docs, vec![0, 1, 2]);
          fs::remove_dir_all(&root).unwrap();
      }

      #[test]
      fn empty_index() {
          let root = temp_dir("empty");
          let mut w = IndexWriter::create(&root, schema(), IndexWriterConfig::default()).unwrap();
          w.commit().unwrap();
          drop(w);
          let dir = FSDirectory::open(&root).unwrap();
          let mut s = Searcher::open(&dir).unwrap();
          assert_eq!(s.max_doc(), 0);
          assert_eq!(s.segment_count(), 0);
          assert_eq!(s.count(&Query::MatchAll).unwrap(), 0);
          assert_eq!(s.count(&Query::term("level", "INFO")).unwrap(), 0);
          let (total, docs) = s.top_docs(&Query::MatchAll, 10).unwrap();
          assert_eq!(total, 0);
          assert!(docs.is_empty());
          fs::remove_dir_all(&root).unwrap();
      }
  }
  ```

  同时 `crates/core/src/lib.rs` 先只加 `pub mod search;`（否则无法编译），模块文档头从：

  ```rust
  //! RustLucene core: Lucene 9.12.3-compatible index write path.
  //!
  //! Append-only writer: buffers documents in RAM, flushes them into segment
  //! files via `codec-lucene9`, and publishes commit points (`segments_N`)
  //! with Lucene's two-phase commit protocol. Read/search/merge are out of
  //! scope (delegated to Java Lucene).
  ```

  改为：

  ```rust
  //! RustLucene core: Lucene 9.12.3-compatible index write and read paths.
  //!
  //! Append-only writer: buffers documents in RAM, flushes them into segment
  //! files via `codec-lucene9`, and publishes commit points (`segments_N`)
  //! with Lucene's two-phase commit protocol. The `search` module reads back
  //! those indexes (Term/MatchAll queries, ConstantScore semantics; see
  //! docs/superpowers/specs/2026-07-22-rust-search-design.md).
  ```

- [ ] **Step 8.2: 跑测试确认失败**

  ```
  $ cargo test -p rustlucene-core search:: 2>&1 | tail -5
  error[E0432]: unresolved import `codec_lucene9::terms_read` ... / no `Searcher` in `search`
  ```
  （`search/collector.rs` 等子模块文件尚不存在，编译失败。）

- [ ] **Step 8.3: 最小实现** —

  `crates/core/src/search/segment_reader.rs`：

  ```rust
  //! Per-segment read view (search spec §3 SegmentReader): field infos +
  //! terms dict + postings of one segment. Open reads only segments_N/.si/.fnm
  //! + codec headers; field FSTs load lazily inside the terms dict.

  use std::io;

  use codec_lucene9::directory::FSDirectory;
  use codec_lucene9::field_infos::{FieldInfos, IndexOptions};
  use codec_lucene9::postings_read::{DocsEnum, DocsFreqsEnum, PostingsReader};
  use codec_lucene9::segment_infos::SegmentCommitInfo;
  use codec_lucene9::terms_read::{TermEntry, TermsDict};

  pub struct SegmentReader {
      max_doc: i32,
      field_infos: FieldInfos,
      terms: TermsDict,
      postings: PostingsReader,
  }

  impl SegmentReader {
      pub fn open(dir: &FSDirectory, sci: &SegmentCommitInfo) -> io::Result<SegmentReader> {
          let segment = &sci.info.name;
          let segment_id = &sci.info.id;
          let field_infos = FieldInfos::read(dir, segment, segment_id, "")?;
          let terms = TermsDict::open(dir, segment, segment_id, &field_infos)?;
          let postings = PostingsReader::open(dir, segment, segment_id)?;
          Ok(SegmentReader {
              max_doc: sci.info.doc_count,
              field_infos,
              terms,
              postings,
          })
      }

      pub fn max_doc(&self) -> i32 {
          self.max_doc
      }

      /// Term lookup: field resolution + terms-dict seek. Returns
      /// `(has_freqs, entry)`; `None` covers unknown field, non-indexed field
      /// (IndexOptions.NONE), and term-not-present — all empty-hit cases,
      /// matching Java TermQuery semantics.
      pub(crate) fn seek_term(
          &mut self,
          field: &str,
          term: &[u8],
      ) -> io::Result<Option<(bool, TermEntry)>> {
          let Some(fi) = self.field_infos.by_name(field) else {
              return Ok(None);
          };
          if fi.index_options == IndexOptions::None {
              return Ok(None);
          }
          let has_freqs = fi.index_options != IndexOptions::Docs;
          let Some(entry) = self.terms.seek_exact(fi, term)? else {
              return Ok(None);
          };
          Ok(Some((has_freqs, entry)))
      }

      pub(crate) fn docs_enum(&self, entry: &TermEntry) -> io::Result<DocsEnum> {
          self.postings.docs(entry)
      }

      pub(crate) fn docs_freqs_enum(&self, entry: &TermEntry) -> io::Result<DocsFreqsEnum> {
          self.postings.docs_and_freqs(entry)
      }
  }
  ```

  `crates/core/src/search/reader.rs`：

  ```rust
  //! Multi-segment reader (search spec §3 reader.rs): parses the latest
  //! segments_N commit and holds the SegmentReader list. Open is a snapshot —
  //! reopen to refresh (spec §1: no NRT).

  use std::io;

  use codec_lucene9::directory::FSDirectory;
  use codec_lucene9::segment_infos::SegmentInfos;

  use super::segment_reader::SegmentReader;

  pub struct Reader {
      segments: Vec<SegmentReader>,
      doc_bases: Vec<i32>,
      max_doc: i32,
  }

  impl Reader {
      /// DirectoryReader.open: reads the latest commit and opens every
      /// segment in commit order (global docID = docBase + segment docID).
      pub fn open(dir: &FSDirectory) -> io::Result<Reader> {
          let (infos, _generation) = SegmentInfos::read_latest(dir)?;
          let mut segments = Vec::with_capacity(infos.segments.len());
          let mut doc_bases = Vec::with_capacity(infos.segments.len());
          let mut base = 0i32;
          for sci in &infos.segments {
              let seg = SegmentReader::open(dir, sci)?;
              doc_bases.push(base);
              base += seg.max_doc();
              segments.push(seg);
          }
          Ok(Reader {
              segments,
              doc_bases,
              max_doc: base,
          })
      }

      pub fn max_doc(&self) -> i32 {
          self.max_doc
      }

      pub fn segment_count(&self) -> usize {
          self.segments.len()
      }

      /// Leaves in commit order with their doc bases (DirectoryReader.leaves).
      pub(crate) fn leaves(&mut self) -> impl Iterator<Item = (i32, &mut SegmentReader)> {
          self.doc_bases.iter().copied().zip(self.segments.iter_mut())
      }
  }
  ```

  `crates/core/src/search/query.rs`：

  ```rust
  //! Query enum (search spec §3): M1 carries Term and MatchAll only. All
  //! queries have ConstantScore semantics — no scoring anywhere.

  use std::io;

  use super::doc_iter::{MatchAllIter, SegmentDocIter};
  use super::segment_reader::SegmentReader;

  #[derive(Clone, Debug, PartialEq, Eq)]
  pub enum Query {
      Term { field: String, term: Vec<u8> },
      MatchAll,
  }

  impl Query {
      pub fn term(field: &str, term: &str) -> Query {
          Query::Term {
              field: field.to_string(),
              term: term.as_bytes().to_vec(),
          }
      }

      /// Builds the per-segment iterator (Lucene leaf-level execution).
      /// `Ok(None)` = no hits in this segment.
      pub(crate) fn segment_iterator(
          &self,
          seg: &mut SegmentReader,
      ) -> io::Result<Option<SegmentDocIter>> {
          match self {
              Query::MatchAll => Ok(Some(SegmentDocIter::All(MatchAllIter::new(seg.max_doc())))),
              Query::Term { field, term } => {
                  let Some((has_freqs, entry)) = seg.seek_term(field, term)? else {
                      return Ok(None);
                  };
                  if has_freqs {
                      Ok(Some(SegmentDocIter::Freqs(seg.docs_freqs_enum(&entry)?)))
                  } else {
                      Ok(Some(SegmentDocIter::Docs(seg.docs_enum(&entry)?)))
                  }
              }
          }
      }
  }
  ```

  `crates/core/src/search/doc_iter.rs`：

  ```rust
  //! DocIdSetIterator semantics (docID starts at -1, ascends, ends at
  //! NO_MORE_DOCS) with a Rust object shape (search spec §3: enum Query +
  //! trait DocIter, no inheritance).

  use std::io;

  use codec_lucene9::postings_read::{DocsEnum, DocsFreqsEnum, NO_MORE_DOCS};

  pub trait DocIter {
      fn doc_id(&self) -> i32;
      fn next_doc(&mut self) -> io::Result<i32>;

      /// M1: linear advance (next_doc loop); skip-data-driven advance is a
      /// later phase (search spec phase 3, Boolean conjunction).
      fn advance(&mut self, target: i32) -> io::Result<i32> {
          if self.doc_id() >= target {
              return Ok(self.doc_id());
          }
          loop {
              let d = self.next_doc()?;
              if d >= target {
                  return Ok(d);
              }
          }
      }

      /// Current doc's term frequency (1 for docs-only iterators).
      fn freq(&self) -> u32 {
          1
      }
  }

  /// MatchAllDocsQuery: [0..maxDoc) scan (spec §1: 段元数据构造验证基线).
  pub struct MatchAllIter {
      doc: i32,
      max_doc: i32,
  }

  impl MatchAllIter {
      pub fn new(max_doc: i32) -> Self {
          MatchAllIter { doc: -1, max_doc }
      }
  }

  impl DocIter for MatchAllIter {
      fn doc_id(&self) -> i32 {
          self.doc
      }

      fn next_doc(&mut self) -> io::Result<i32> {
          if self.doc == NO_MORE_DOCS {
              return Ok(NO_MORE_DOCS);
          }
          self.doc += 1;
          if self.doc >= self.max_doc {
              self.doc = NO_MORE_DOCS;
          }
          Ok(self.doc)
      }

      fn advance(&mut self, target: i32) -> io::Result<i32> {
          if target > self.doc {
              self.doc = if target >= self.max_doc {
                  NO_MORE_DOCS
              } else {
                  target
              };
          }
          Ok(self.doc)
      }
  }

  /// Per-segment iterators (enum dispatch, no boxing).
  pub enum SegmentDocIter {
      Docs(DocsEnum),
      Freqs(DocsFreqsEnum),
      All(MatchAllIter),
  }

  impl DocIter for SegmentDocIter {
      fn doc_id(&self) -> i32 {
          match self {
              SegmentDocIter::Docs(d) => d.doc_id(),
              SegmentDocIter::Freqs(f) => f.doc_id(),
              SegmentDocIter::All(a) => a.doc_id(),
          }
      }

      fn next_doc(&mut self) -> io::Result<i32> {
          match self {
              SegmentDocIter::Docs(d) => d.next_doc(),
              SegmentDocIter::Freqs(f) => f.next_doc(),
              SegmentDocIter::All(a) => a.next_doc(),
          }
      }

      fn advance(&mut self, target: i32) -> io::Result<i32> {
          match self {
              SegmentDocIter::Docs(d) => d.advance(target),
              SegmentDocIter::Freqs(f) => f.advance(target),
              SegmentDocIter::All(a) => a.advance(target),
          }
      }

      fn freq(&self) -> u32 {
          match self {
              SegmentDocIter::Freqs(f) => f.freq(),
              _ => 1,
          }
      }
  }
  ```

  `crates/core/src/search/collector.rs`：

  ```rust
  //! Collectors (search spec §3). M1 is per-doc callback only; the block-level
  //! batch interface (spec §4b DocBlock) arrives with the SIMD phase.

  pub trait Collector {
      /// `doc` is the global docID (docBase applied), `freq` the term freq
      /// (1 for docs-only iterators).
      fn collect(&mut self, doc: i32, freq: u32);
  }

  /// Total hit count (diff battery workhorse).
  #[derive(Default)]
  pub struct CountCollector {
      pub count: u64,
  }

  impl Collector for CountCollector {
      fn collect(&mut self, _doc: i32, _freq: u32) {
          self.count += 1;
      }
  }

  /// Top-N by docID (Sort.INDEXORDER semantics): hits ascend in docID during
  /// the drive, so the top N is exactly the first N hits; `total` still
  /// counts everything.
  pub struct TopDocCollector {
      top_n: usize,
      pub total: u64,
      pub docs: Vec<i32>,
  }

  impl TopDocCollector {
      pub fn new(top_n: usize) -> Self {
          TopDocCollector {
              top_n,
              total: 0,
              docs: Vec::with_capacity(top_n.min(1024)),
          }
      }
  }

  impl Collector for TopDocCollector {
      fn collect(&mut self, doc: i32, _freq: u32) {
          self.total += 1;
          if self.docs.len() < self.top_n {
              self.docs.push(doc);
          }
      }
  }

  /// Sum of term freqs over all hits — exercises the PFor freq decode end to
  /// end (used by the Java diff battery).
  #[derive(Default)]
  pub struct FreqSumCollector {
      pub total_freq: u64,
  }

  impl Collector for FreqSumCollector {
      fn collect(&mut self, _doc: i32, freq: u32) {
          self.total_freq += freq as u64;
      }
  }
  ```

  `crates/core/src/search/searcher.rs`：

  ```rust
  //! IndexSearcher (search spec §3): query × segment → collector. Single
  //! threaded, segment-sequential (spec §1: 查询并发不做，段间并行仅留接口).

  use std::io;

  use codec_lucene9::directory::FSDirectory;
  use codec_lucene9::postings_read::NO_MORE_DOCS;

  use super::collector::{Collector, CountCollector, FreqSumCollector, TopDocCollector};
  use super::doc_iter::DocIter;
  use super::query::Query;
  use super::reader::Reader;

  pub struct Searcher {
      reader: Reader,
  }

  impl Searcher {
      pub fn open(dir: &FSDirectory) -> io::Result<Searcher> {
          Ok(Searcher {
              reader: Reader::open(dir)?,
          })
      }

      pub fn max_doc(&self) -> i32 {
          self.reader.max_doc()
      }

      pub fn segment_count(&self) -> usize {
          self.reader.segment_count()
      }

      /// Drives the query per segment and feeds global docIDs to the
      /// collector (leaf-level execution + docBase mapping).
      pub fn search<C: Collector>(&mut self, query: &Query, collector: &mut C) -> io::Result<()> {
          for (doc_base, seg) in self.reader.leaves() {
              let Some(mut iter) = query.segment_iterator(seg)? else {
                  continue;
              };
              loop {
                  let doc = iter.next_doc()?;
                  if doc == NO_MORE_DOCS {
                      break;
                  }
                  collector.collect(doc_base + doc, iter.freq());
              }
          }
          Ok(())
      }

      pub fn count(&mut self, query: &Query) -> io::Result<u64> {
          let mut c = CountCollector::default();
          self.search(query, &mut c)?;
          Ok(c.count)
      }

      /// (total hits, first `n` docIDs ascending) — Sort.INDEXORDER topN.
      pub fn top_docs(&mut self, query: &Query, n: usize) -> io::Result<(u64, Vec<i32>)> {
          let mut c = TopDocCollector::new(n);
          self.search(query, &mut c)?;
          Ok((c.total, c.docs))
      }

      pub fn freq_sum(&mut self, query: &Query) -> io::Result<u64> {
          let mut c = FreqSumCollector::default();
          self.search(query, &mut c)?;
          Ok(c.total_freq)
      }
  }
  ```

- [ ] **Step 8.4: 跑测试确认通过**

  ```
  $ cargo test -p rustlucene-core search:: 2>&1 | tail -3
  test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
  $ cargo test 2>&1 | grep -E "^test result"   # 全 workspace 回归
  test result: ok. 124 passed; 0 failed; ...（codec）
  test result: ok. 30 passed; 0 failed; ...（core，含 4 个新测试）
  ```

- [ ] **Step 8.5: 提交**

  ```
  git add crates/core/src/search/ crates/core/src/lib.rs
  git commit -m "feat: core search module (Reader/SegmentReader/Query/DocIter/collectors/Searcher)"
  ```

---

## Task 9: Java diff 验证电池第一版（纳入 make log-test）

Rust 搜索结果 vs Java Lucene 9.12.3 读取结果逐条 diff（spec §6 第三层，决定性验收）。电池覆盖 M1 全部读路径：keyword term（DOCS，df≈4 万 → level-1 skip 跨界）、text term（DOCS_AND_FREQS，多块 + tail + PFor freq）、df=1 singleton（trace_id）、not-found、MatchAll、docID 序 topN。

集成方式：`interop/verify-log.sh` 末尾追加一次 `interop/verify-search.sh` 调用——`make log-test` 的 4 个语料变体（默认 / --positions / --sparse / --bigdict，Makefile:17-22）自动带上搜索 diff，**Makefile 无需改动**。

**Files:**
- Create: `interop/java/VerifySearchIndex.java`
- Create: `interop/verify-search.sh`
- Modify: `crates/core/src/bin/rustlucene-cli.rs`（新增 `searchdump` 子命令）
- Modify: `interop/verify-log.sh`（末尾追加搜索 diff 调用）
- Test: 端到端即测试（`interop/verify-log.sh` 全绿）；单元层面由 T1–T8 的测试覆盖

**Interfaces:**
- Consumes: T8 的 `Searcher` / `Query`（core `search` 模块）；CLI 既有 `vocab()` / `gen_log_document` / `XorShift`（trace_id 重放）；`make java-classes` 编译全部 `interop/java/*.java`。
- Produces:
  ```
  rustlucene-cli searchdump <indexDir> <numDocs> <seed>   # stdout 电池输出
  interop/verify-search.sh <rustIndexDir> <javaIndexDir> <numDocs> <seed>
  ```
  两侧输出逐行相同，格式（示例）：
  ```
  maxDoc=200000
  term level=INFO count=39977
  ...
  term level=INFO first20=3,8,17,...
  term message=connection0 count=1987 freqsum=2034
  term message=nosuchterm42 count=0
  term trace_id(doc7)=8f3a... count=1
  matchall count=200000
  matchall first20=0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,
  ```

### Steps

- [ ] **Step 9.1: 写 Java 侧基准 `interop/java/VerifySearchIndex.java`**（完整新文件；沿用 VerifyLogIndex 的输出风格，所有 count 查询包 ConstantScoreQuery）：

  ```java
  import java.nio.file.*;
  import org.apache.lucene.document.*;
  import org.apache.lucene.index.*;
  import org.apache.lucene.search.*;
  import org.apache.lucene.store.*;
  import org.apache.lucene.util.BytesRef;

  /**
   * Search battery for the M1 Rust read path: fixed Term/MatchAll queries with
   * results printed in the exact format of `rustlucene-cli searchdump`.
   * Diffing the two outputs validates the Rust reader against Java Lucene
   * 9.12.3 (counts, docID sequences, freq sums, singleton df=1 path).
   *
   * Usage: VerifySearchIndex <indexDir>
   */
  public class VerifySearchIndex {
      public static void main(String[] args) throws Exception {
          Path indexDir = Paths.get(args[0]);
          StringBuilder out = new StringBuilder();

          try (Directory dir = FSDirectory.open(indexDir);
               IndexReader r = DirectoryReader.open(dir)) {
              IndexSearcher s = new IndexSearcher(r);
              out.append("maxDoc=").append(r.maxDoc()).append('\n');

              // keyword terms (IndexOptions.DOCS), df ~40k crosses the 4096-doc
              // level-1 skip boundary
              for (String level : new String[]{"INFO","WARN","ERROR","DEBUG","TRACE"}) {
                  Query q = new ConstantScoreQuery(new TermQuery(new Term("level", level)));
                  out.append("term level=").append(level)
                     .append(" count=").append(s.count(q)).append('\n');
              }
              {
                  Query q = new ConstantScoreQuery(new TermQuery(new Term("level", "INFO")));
                  TopDocs td = s.search(q, 20, Sort.INDEXORDER);
                  StringBuilder b = new StringBuilder();
                  for (ScoreDoc sd : td.scoreDocs) b.append(sd.doc).append(',');
                  out.append("term level=INFO first20=").append(b).append('\n');
              }

              // text terms (DOCS_AND_FREQS): count + full freq sum
              for (String w : new String[]{"connection0","query23","queue39"}) {
                  Query q = new ConstantScoreQuery(new TermQuery(new Term("message", w)));
                  long freqsum = 0;
                  PostingsEnum pe = MultiTerms.getTermPostingsEnum(
                      r, "message", new BytesRef(w), PostingsEnum.FREQS);
                  if (pe != null) {
                      while (pe.nextDoc() != PostingsEnum.NO_MORE_DOCS) freqsum += pe.freq();
                  }
                  out.append("term message=").append(w)
                     .append(" count=").append(s.count(q))
                     .append(" freqsum=").append(freqsum).append('\n');
              }
              {
                  Query q = new ConstantScoreQuery(new TermQuery(new Term("message", "nosuchterm42")));
                  out.append("term message=nosuchterm42 count=").append(s.count(q)).append('\n');
              }

              // df=1 singleton: doc7's trace_id from stored fields (ground truth)
              StoredFields stored = s.storedFields();
              if (r.maxDoc() > 7) {
                  String tid = stored.document(7).get("trace_id");
                  Query q = new ConstantScoreQuery(new TermQuery(new Term("trace_id", tid)));
                  out.append("term trace_id(doc7)=").append(tid)
                     .append(" count=").append(s.count(q)).append('\n');
              }

              {
                  Query q = new MatchAllDocsQuery();
                  out.append("matchall count=").append(s.count(q)).append('\n');
                  TopDocs td = s.search(q, 20, Sort.INDEXORDER);
                  StringBuilder b = new StringBuilder();
                  for (ScoreDoc sd : td.scoreDocs) b.append(sd.doc).append(',');
                  out.append("matchall first20=").append(b).append('\n');
              }
          }
          System.out.print(out);
      }
  }
  ```

- [ ] **Step 9.2: 写 Rust 侧 `searchdump` 子命令** — `crates/core/src/bin/rustlucene-cli.rs`：

  顶部 `use rustlucene_core::{...}` 的导入列表中追加 `search::{Query, Searcher}`（独立一行 `use rustlucene_core::search::{Query, Searcher};`）。在 `fn logbench` 之前插入：

  ```rust
  /// Search battery over a log-corpus index, printed line by line in the exact
  /// format of interop/java/VerifySearchIndex.java — the two outputs are
  /// diffed by interop/verify-search.sh (make log-test).
  fn searchdump(index_dir: &Path, num_docs: u32, seed: u64) -> std::io::Result<()> {
      let dir = FSDirectory::open(index_dir)?;
      let mut searcher = Searcher::open(&dir)?;
      let mut out = String::new();
      out.push_str(&format!("maxDoc={}\n", searcher.max_doc()));

      for level in LEVELS {
          let count = searcher.count(&Query::term("level", level))?;
          out.push_str(&format!("term level={level} count={count}\n"));
      }
      let (_, docs) = searcher.top_docs(&Query::term("level", "INFO"), 20)?;
      out.push_str(&format!("term level=INFO first20={}\n", doc_csv(&docs)));

      for w in ["connection0", "query23", "queue39"] {
          let q = Query::term("message", w);
          let count = searcher.count(&q)?;
          let freqsum = searcher.freq_sum(&q)?;
          out.push_str(&format!("term message={w} count={count} freqsum={freqsum}\n"));
      }
      let count = searcher.count(&Query::term("message", "nosuchterm42"))?;
      out.push_str(&format!("term message=nosuchterm42 count={count}\n"));

      if num_docs > 7 {
          let tid = trace_id_of_doc(seed, 7);
          let count = searcher.count(&Query::term("trace_id", &tid))?;
          out.push_str(&format!("term trace_id(doc7)={tid} count={count}\n"));
      }

      let count = searcher.count(&Query::MatchAll)?;
      out.push_str(&format!("matchall count={count}\n"));
      let (_, docs) = searcher.top_docs(&Query::MatchAll, 20)?;
      out.push_str(&format!("matchall first20={}\n", doc_csv(&docs)));
      print!("{out}");
      Ok(())
  }

  fn doc_csv(docs: &[i32]) -> String {
      let mut s = String::new();
      for d in docs {
          s.push_str(&d.to_string());
          s.push(',');
      }
      s
  }

  /// Replays the log corpus generator (same RNG stream as logwrite; the
  /// sparse/bigdict flags only gate whether fields are *added*, the draws are
  /// identical) to recover doc `n`'s trace_id without reading stored fields.
  fn trace_id_of_doc(seed: u64, n: u64) -> String {
      let vocab = vocab();
      let mut rng = XorShift::new(seed);
      let mut tid = String::new();
      for doc_id in 0..=n {
          let doc = gen_log_document(&mut rng, &vocab, doc_id, false, false);
          tid = match doc.fields.iter().find(|(name, _)| name == "trace_id") {
              Some((_, FieldValue::Keyword(k))) => k.clone(),
              _ => panic!("trace_id must be a keyword field"),
          };
      }
      tid
  }
  ```

  `fn main()` 的 `match args[1].as_str()` 中 `"logbench"` 分支之后追加分支：

  ```rust
          "searchdump" => {
              if args.len() < 5 {
                  usage();
              }
              searchdump(
                  Path::new(&args[2]),
                  args[3].parse().unwrap(),
                  args[4].parse().unwrap(),
              )
          }
  ```

  `usage()` 函数（`fn usage` 内现有 eprintln 列表）追加一行：

  ```rust
      eprintln!("  rustlucene-cli searchdump <indexDir> <numDocs> <seed>");
  ```

- [ ] **Step 9.3: 写 `interop/verify-search.sh`**（完整新文件，`chmod +x`）：

  ```bash
  #!/usr/bin/env bash
  # M1 search diff: rustlucene-cli searchdump (Rust read path) vs
  # VerifySearchIndex (Java Lucene 9.12.3) over the same two indexes.
  # Called by interop/verify-log.sh after both indexes are built.
  # Usage: interop/verify-search.sh <rustIndexDir> <javaIndexDir> <numDocs> <seed>
  set -euo pipefail

  ROOT="$(cd "$(dirname "$0")/.." && pwd)"
  RUST_DIR="$1"
  JAVA_DIR="$2"
  NUM_DOCS="$3"
  SEED="$4"
  CP="$ROOT/interop/java/classes:$ROOT/interop/java/lib/lucene-core-9.12.3.jar:$ROOT/interop/java/lib/lucene-analysis-common-9.12.3.jar"

  echo "== Rust: searchdump"
  cargo run -q --release -p rustlucene-core --bin rustlucene-cli -- \
    searchdump "$RUST_DIR" "$NUM_DOCS" "$SEED" > /tmp/rl-search-rust.out

  echo "== Java: VerifySearchIndex"
  java -cp "$CP" VerifySearchIndex "$JAVA_DIR" > /tmp/rl-search-java.out

  diff -u /tmp/rl-search-rust.out /tmp/rl-search-java.out
  cat /tmp/rl-search-rust.out

  echo "SEARCH_INTEROP_OK"
  ```

- [ ] **Step 9.4: 修改 `interop/verify-log.sh`** — 在 `cat /tmp/rl-log-rust.out` 之后、`echo "LOG_INTEROP_OK"` 之前插入：

  ```bash
  echo "== Search diff: searchdump vs VerifySearchIndex"
  "$ROOT/interop/verify-search.sh" "$RUST_DIR" "$JAVA_DIR" "$NUM_DOCS" "$SEED"
  ```

- [ ] **Step 9.5: 编译 + 端到端验证（决定性验收）**

  ```
  $ cargo build --release 2>&1 | tail -2
      Finished `release` profile [optimized] target(s)
  $ make java-classes 2>&1 | tail -2
      （javac 无输出即成功）
  $ interop/verify-log.sh 200000 42
  == Rust: logwrite (200000 docs, seed 42 )
  ...
  == CheckIndex /tmp/rl-log-rust
  No problems found
  == VerifyLogIndex: Rust vs Java dumps
  ...
  == Search diff: searchdump vs VerifySearchIndex
  == Rust: searchdump
  == Java: VerifySearchIndex
  maxDoc=200000
  term level=INFO count=39...
  ...
  matchall first20=0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,
  SEARCH_INTEROP_OK
  LOG_INTEROP_OK
  ```
  快速烟测（迭代时用更小语料）：`interop/verify-log.sh 20000 42`。
  注意：df≈4 万的 level term 只在 200000 语料下跨 4096 的 level-1 边界——`make log-test` 的默认语料量已覆盖该路径，验收以 `make log-test` 四条变体全绿为准：

  ```
  $ make log-test
      （4 条 verify-log.sh 变体依次跑过，每条含 CheckIndex + VerifyLogIndex diff + SEARCH_INTEROP_OK）
  ```

- [ ] **Step 9.6: 提交**

  ```
  git add interop/java/VerifySearchIndex.java interop/verify-search.sh interop/verify-log.sh crates/core/src/bin/rustlucene-cli.rs
  git commit -m "feat: search diff battery (searchdump + VerifySearchIndex + verify-search.sh in log-test)"
  ```

---

## 完成定义（M1）

1. `cargo build` 与 `cargo test`（全 workspace）绿：codec-lucene9 既有 90 测试 + T1–T7 新增约 30 测试，core 既有测试 + T8 新增 4 测试。
2. `make log-test` 绿：4 个语料变体上，Rust 读路径的 term/matchall 搜索结果与 Java Lucene 9.12.3 逐字节一致（`SEARCH_INTEROP_OK` × 4）。
3. 覆盖矩阵（spec §6 边界语料）中 M1 相关行：空索引（T8 `empty_index`）、单文档/单 term df=1（T7 singleton + 电池 trace_id）、高 df term 多块 + tail 与 level-1 跨界（T7 df=5000 + 电池 level）、多段索引（T8 `multi_segment_docbase_mapping`）。

## 后续阶段接口预留（不在本计划实现，仅记录锚点）

- `for_util_decode` / `pfor_util_decode` 的 `[u64; BLOCK_SIZE]` 块形状 = SIMD bit-unpack 核的接入点（spec §4a）。
- `DocIter::advance` 当前为线性循环；skip-data 驱动版在阶段 3（Boolean 合取）实现，`EnumCore` 已持有 level-0/level-1 全部状态（`level1_doc_end_fp` / `level1_last_doc` / `level0_last_doc`），届时按 `skipLevel0To`（Lucene912PostingsReader :548-571）扩展即可。
- `TermsDict` 的顺序枚举（`next()`）在阶段 7（Prefix/Wildcard）扩展：block 装载逻辑（`scan_block` 的 loadBlock 部分）可直接复用。
- positions（.pos）、DV（.dvd/.dvm）、BKD（.kdd/.kdi）、stored（.fdt/.fdx）读：各自 `*_read.rs` 新增，与 `terms_read` / `postings_read` 同构（open 时 header + length/footer 校验，`SegmentReader::open` 挂载）。
