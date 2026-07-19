调研完成，所有结论均出自本地 9.12.3 源码（路径前缀 `$SRC` = `lucene/core/src/java/org/apache/lucene`）。一个重要发现先说：**9.12.3 的默认 codec 是 `Lucene912`（`codecs/Codec.java:58`），它装配的是 `Lucene94FieldInfosFormat` + `Lucene99SegmentInfoFormat`，不是任务里假设的 Lucene90 版本**（`codecs/lucene912/Lucene912Codec.java:71-76,108-124`）。

---

## 1. CodecUtil 头/尾精确布局（`codecs/CodecUtil.java`）

**writeIndexHeader(out, codec, version, id, suffix)**（121-135）：
- `writeBEInt(CODEC_MAGIC)`：4 字节**大端** `0x3fd76c17`（46；writeBEInt 653-658）
- `writeString(codec)`：VInt 字节长度 + UTF-8 字节（ASCII <128，77-86）
- `writeBEInt(version)`：4 字节大端
- ObjectID：原样 16 字节（`StringHelper.ID_LENGTH=16`，`util/StringHelper.java:294`；来自 `StringHelper.randomId()`，297-329）
- suffix：1 字节长度 + ASCII 字节（<256，128-134）
- 长度：`headerLength=9+len(codec)`（144），`indexHeaderLength=headerLength+17+len(suffix)`（155）

**writeFooter(out)**（409-413）：`writeBEInt(FOOTER_MAGIC)` + `writeBEInt(0)` + `writeBELong(crc)`，共 16 字节（footerLength 421）。
- `FOOTER_MAGIC = ~CODEC_MAGIC = 0xC02893E8`（49）
- algorithmID 恒为 0 = zlib CRC32（javadoc 401；算法实现 `java.util.zip.CRC32`，`store/BufferedChecksumIndexInput.java:20,34`）
- **CRC 覆盖范围：从文件第 0 字节到 algorithmID 为止的全部字节（含 FOOTER_MAGIC 与 algorithmID 这 8 字节本身）**，写入时取 `output.getChecksum()`（writeCRC 643-650），高 32 位必须为 0。读取端 checkFooter 在读完 footer 前 8 字节后比较（432-445）。

**容错**：magic 不符→CorruptIndexException；codec 名不符→CorruptIndexException；version<min→IndexFormatTooOldException，>max→IndexFormatTooNewException（182-218）。checkIndexHeader 额外逐字节校验 16 字节 ID 和 suffix（246-258, 363-389）。validateFooter 要求剩余正好 16 字节、magic、algorithmID==0（560-598）。checksumEntireFile：clone seek(0)，BufferedChecksumIndexInput 流式读完再 checkFooter（606-621）。

## 2. DataOutput 基础编码（`store/DataOutput.java`）

- **writeInt / writeLong 是小端（LE）**（73-78, 223-226）！大端只出现在 CodecUtil.writeBEInt/writeBELong（CodecUtil.java:652-677）——segments_N 的 version/delGen 等、各文件 header/footer 的 magic/version/CRC 用 BE；`.si` 里的 version ints、docCount、`.fnm` 的 dvGen、`.fdm` 的 ints/longs 都是 LE。
- writeVInt（198-204）：7 位/组，低位组在前，续位 0x80；writeVLong 同构，禁止负数（236-250）；writeZInt/ZLong = zigzag+VInt/VLong（213-215, 259-261）。
- writeString（271-275）：VInt **字节**长度 + **标准 UTF-8**（`UnicodeUtil.UTF16toUTF8`，`util/BytesRef.java:79-82`），**不是 modified UTF-8**。
- writeMapOfStrings：VInt size + (key,value) writeString 对（304-310）；writeSetOfStrings：VInt size + writeString（321-326）。
- **GroupVInt**：存在于 `util/GroupVIntUtil.java`（不在 store/）。块 = 1 字节 flag（每 2 bit 存 nBytes-1：bit7-6→v1 … bit1-0→v4）+ 4 个 LE 的 1/2/3/4 字节值，MAX_LENGTH_PER_GROUP=17（28-67, 134-163）；不足 4 个的尾部用普通 VInt。`DataOutput.writeGroupVInts`（337-342）。**只被 `codecs/lucene912/PostingsUtil.java:38,63`（Lucene912PostingsFormat 的 .doc）使用；stored fields、segments_N、.si、.fnm、norms 均不用**。

## 3. segments_N（`index/SegmentInfos.java`）

- format 名 `"segments"`；VERSION_70=7、VERSION_72=8、VERSION_74=9、VERSION_86=10、**VERSION_CURRENT=10**（120-131）。
- 写出布局（write 596-699，读取 321-477 严格对称）：
  1. writeIndexHeader("segments", 10, `randomId()`, `Long.toString(generation, 36)`) —— suffix 是 36 进制的 generation（597-602）
  2. 3×VInt：Version.LATEST = **9,12,3**（603-605；`util/Version.java:347,363`）
  3. VInt indexCreatedVersionMajor（写 9 即可；读取端要求 ≤ luceneVersion.major 且 ≥ MIN_SUPPORTED_MAJOR=8，345-369；`Version.java:376`）
  4. **BE Long** version；**VLong** counter；**BE Int** numSegments（611-613）
  5. numSegments>0 时 3×VInt minSegmentLuceneVersion（615-631）
  6. 每 segment：String name（如 `"_0"`）；16 字节 segmentID；String codec 名（`"Lucene912"`，读取时 `Codec.forName`，521-524）；BE Long delGen；BE Int delCount；BE Long fieldInfosGen；BE Long docValuesGen；BE Int softDelCount；SCI-ID：marker 字节 1 + 16 字节 id（或 0=null，681-688）；writeSetOfStrings(fieldInfosFiles)；BE Int dvUpdates 字段数 + 每字段（BE Int fieldNumber + writeSetOfStrings)（690-696）
  7. writeMapOfStrings(userData)（698）；writeFooter（699）
- **两段式提交**：prepareCommit（915-921）先 `dir.syncMetaData()`，再 write(dir)：写 `pending_segments_N`（PENDING_SEGMENTS="pending_segments"，`index/IndexFileNames.java:43`），close 后 `directory.sync([file])`（563-593）。finishCommit（945-977）：`dir.rename(pending_segments_N → segments_N)`，随后再次 `dir.syncMetaData()`；失败回滚删 pending（891-905）。generation：next = generation+1（初始 -1→1）（269-275）；文件名 `segments_` + base36(gen)（`IndexFileNames.fileNameFromGeneration:55-75`，gen=0 时无后缀）。
- **读取选择**：FindSegmentsFile.run（782-853）：`listAll()` 取两次且必须一致；`getLastCommitGeneration` 取以 "segments" 开头（排除 "segments.gen"）的最大 gen（201-215）；对 segments_gen 执行 doBody；IOException 时仅当 gen 前进才重试，否则抛原始异常；gen=-1 抛 IndexNotFoundException。

## 4. .si（`codecs/lucene99/Lucene99SegmentInfoFormat.java`；9.12 默认走 lucene99 包）

- CODEC_NAME = **"Lucene90SegmentInfo"**，VERSION_START=VERSION_CURRENT=**0**（83-85），扩展名 "si"，suffix ""。
- 布局（writeSegmentInfo 185-235）：
  - indexHeader
  - 3×**LE Int** version.major/minor/bugfix（192-194）→ 9,12,3
  - 1 字节 hasMinVersion：1 + 3×LE Int（197-205）。**必须写 1**（indexCreatedVersionMajor≥7 时 minVersion 不能为 null，SegmentInfos.java:636-640）
  - LE Int docCount（208）
  - 1 字节 isCompoundFile：`SegmentInfo.YES=1` / `SegmentInfo.NO=-1`（210；`index/SegmentInfo.java:45-48`）
  - 1 字节 hasBlocks（211）
  - writeMapOfStrings(diagnostics)（212）
  - writeSetOfStrings(files)（220）——必须全部以该 segment 名为前缀（214-219）
  - writeMapOfStrings(attributes)（221）——**必须含 key `"Lucene90StoredFieldsFormat.mode"`=`"BEST_SPEED"`**，否则 fieldsReader 抛 IllegalStateException（`Lucene90StoredFieldsFormat.java:113-114,131-137,142`）
  - VInt numSortFields（无 sort 写 0）+ 每 field（String providerName + SortFieldProvider 字节流）（223-234）
  - writeFooter
- segment id 由写入方生成 `StringHelper.randomId()`（`IndexWriter.java:3427,5010`、`DocumentsWriterPerThread.java:176,467`）；段名 `"_" + base36(counter++)`（`IndexWriter.java:2051-2064`）。

## 5. .fnm（`codecs/lucene94/Lucene94FieldInfosFormat.java`）

- CODEC_NAME = **"Lucene94FieldInfos"**，FORMAT_START=0、FORMAT_PARENT_FIELD=1、**FORMAT_CURRENT=1**（422-426），扩展名 "fnm"（419），suffix ""。
- 布局（write 367-416）：indexHeader；VInt FieldsCount；每字段：
  - writeString(name)、VInt(fieldNumber)（388-389）
  - **1 字节 bits**（391-397, 429-433）：0x1 STORE_TERMVECTOR、**0x2 OMIT_NORMS**、0x4 STORE_PAYLOADS、0x8 SOFT_DELETES_FIELD、0x10 PARENT_FIELD_FIELD。omitNorms=true 即置 0x2；高 3 位必须 0（读取端校验 167-175）
  - 1 字节 IndexOptions（331-347）：NONE=0、DOCS=1、DOCS_AND_FREQS=2、DOCS_AND_FREQS_AND_POSITIONS=3、DOCS_AND_FREQS_AND_POSITIONS_AND_OFFSETS=4
  - 1 字节 DocValuesType（243-261）：NONE=0、NUMERIC=1、BINARY=2、SORTED=3、SORTED_SET=4、SORTED_NUMERIC=5（9.12 无 -1 编码，就是一个字节；doc 注释里“高低 4bit”的说法已过时，代码是整字节）
  - **LE Long** dvGen（无更新写 -1）（403）
  - writeMapOfStrings(attributes)（404）
  - VInt pointDataDimensionCount；≠0 时再 VInt pointIndexDimensionCount + VInt pointNumBytes（405-409）
  - VInt vectorDimension；1 字节 vectorEncoding 序数（0=BYTE,1=FLOAT32）；1 字节 vectorSimilarity（0=EUCLIDEAN,1=DOT_PRODUCT,2=COSINE,3=MAXIMUM_INNER_PRODUCT，301-322, 410-412）
  - writeFooter。字段写入后 `fi.checkConsistency()`（386）。

## 6. stored fields（BEST_SPEED = LZ4 默认）

装配：`Lucene90StoredFieldsFormat.Mode.BEST_SPEED` → `Lucene90CompressingStoredFieldsFormat("Lucene90StoredFieldsFastData", LZ4WithPresetDictCompressionMode, chunkSize=81920, maxDocsPerChunk=1024, blockShift=10)`（`Lucene90StoredFieldsFormat.java:157-172,182-185`；chunkSize=10*8*1024，**不是 16384**）。

- 文件/常量（`compressing/Lucene90CompressingStoredFieldsWriter.java`）：扩展名 fdt/fdx/fdm（59-65）；INDEX_CODEC_NAME="Lucene90FieldsIndex"（68）→ fdm codec 名 `"Lucene90FieldsIndexMeta"`、fdx `"Lucene90FieldsIndexIdx"`；**VERSION_START=VERSION_CURRENT=1**（80-82，fdt/fdm 都写 1；fdm 读取接受 [META_VERSION_START=0, fdt version]，reader 141-147）；FieldsIndexWriter VERSION=0（`FieldsIndexWriter.java:48-49`）。
- **chunk 语义**：`bufferedDocs.size() >= 81920 || numBufferedDocs >= 1024` 触发 flush（228-232）。
- **chunk header 精确布局**（writeHeader 207-226）：`VInt docBase`；`VInt((numBufferedDocs<<2) | dirtyBit | slicedBit)`（slicedBit=1, dirtyBit=2，215-219）；numStoredFields 数组；lengths 数组（endOffsets 差分，243-248）。
  - 数组编码 saveInts（199-205）：长度==1 → 单个 VInt；否则 `StoredFieldsInts.writeInts`：全相等 → 字节 0 + VInt；否则字节 8/16/32（bits/value），128 个一组按 stride 交叉打包进 long 写出，尾部裸 byte/short/int（`StoredFieldsInts.java:31-115`）。
- **sliced**：`bufferedDocs.size() >= 2*chunkSize`（249）→ 按 chunkSize 切片逐片 compress（254-261），否则整块一次 compress（263）。
- **dirty chunk**：仅 finish 时强制 flush 的最后不完整块（237-239, 473-476），dirtyBit 置位并累计 numDirtyChunks/numDirtyDocs。普通单段 flush：最后一个 chunk dirty=1，numChunks≥1、numDirtyChunks=1、numDirtyDocs=末块文档数（无文档时全 0）。
- **LZ4 压缩块格式**（`LZ4WithPresetDictCompressionMode.java:165-195`）：
  - `VInt dictLength`（= min(65536, len/(10*2))，174-175）
  - `VInt blockLength`（= ceil((len-dictLength)/10)）
  - 先写全部长度：dict 的压缩长度 VInt + 每个 sub-block 的压缩长度 VInt（doCompress 165-170）
  - 再写拼接的压缩字节流（194）。**存的是压缩后长度；原长由 dictLength/blockLength 推出；不可压缩时无 raw 逃逸，LZ4 退化为纯 literal 序列（开销 <0.5%）**
  - sub-block 用前 dict 字节做 preset dictionary（compressWithDictionary）。
- **.fdt**：indexHeader("Lucene90StoredFieldsFastData", 1, si.id, "") + chunks + writeFooter（132-143, 488）。
- **.fdx**（FieldsIndexWriter.finish 106-182）：indexHeader("Lucene90FieldsIndexIdx", 0) + 两个 **DirectMonotonic** 数组的数据区 + writeFooter。数组 1 = chunk 累积 docBase（0, d1, d1+d2, …, totalDocs，共 totalChunks+1 个值，128-136）；数组 2 = chunk startPointer 累积值 + 末尾 maxPointer（156-167）。中间 docBase delta/startPointer delta 先写临时文件再转 DirectMonotonic（临时文件 finish 后删除，83-88,146,174）。DirectMonotonic 每 ≤2^blockShift 值一块，meta 写 **.fdm**：`Long min + Int floatBits(avgInc) + Long dataOffset(相对) + Byte bpv`，数据为 DirectWriter packed deltas（`DirectMonotonicWriter.java:77-114`）。
- **.fdm 精确顺序**（writer 156 + FieldsIndexWriter.finish 118-178 + writer finish 483-487；经 `FieldsIndexReader.java:57-65` 与 reader 149-175 双向确认）：
  1. indexHeader("Lucene90FieldsIndexMeta", 1)
  2. **VInt chunkSize**（81920）
  3. LE Int numDocs；LE Int blockShift(10)；LE Int totalChunks+1
  4. LE Long docsStartPointer（docs DM 数据在 .fdx 的起始偏移）
  5. docs DM meta（每块 29 字节：Long min, Int avgIncBits, Long offset, Byte bpv）
  6. LE Long startPointersStartPointer
  7. filePointers DM meta
  8. LE Long startPointersEndPointer
  9. LE Long maxPointer（fdt 数据末尾=filePointer，footer 之前）
  10. VLong numChunks；VLong numDirtyChunks；VLong numDirtyDocs
  11. writeFooter
- **文档内字段序列化**（272-328）：每字段 `VLong(fieldNumber << 3 | type)`（TYPE_BITS=3，77-78），type：STRING=0（+writeString）、BYTE_ARR=1（+VInt len+bytes）、NUMERIC_INT=2（+ZInt）、NUMERIC_FLOAT=3（+ZFloat，357-374）、NUMERIC_LONG=4（+writeTLong，442-470：2bit 单位 00 原值/01 秒/10 时/11 天 + 续位 0x20 + zigzag 低 5 位 + 余量 VLong）、NUMERIC_DOUBLE=5（+ZDouble，392-415）。

## 7. Norms 空段行为

- `Lucene90NormsConsumer` 构造函数一旦调用就**无条件创建 .nvd/.nvm**（`Lucene90NormsConsumer.java:38-66`）；close 时 meta 写 LE Int `-1` EOF 标记 + 两文件各 writeFooter（69-88）。
- **但全段 omitNorms 时根本不构造 consumer**：`IndexingChain.writeNorms` 只在 `state.fieldInfos.hasNorms()` 为真时才调 `normsFormat.normsConsumer(state)`（`IndexingChain.java:466-487`）→ **不产生 .nvm/.nvd 任何文件**。
- 读取端对称：`SegmentCoreReaders` 只在 `coreFieldInfos.hasNorms()` 时才 normsProducer，容忍文件缺失（`SegmentCoreReaders.java:128-133`）。
- 结论：Rust 端全 omitNorms 段**不要**写 .nvm/.nvd，且不要把它们列入 .si files。有 norms 的段里，无 norm 值的字段条目：Int fieldNumber + Long(-2) + Long(0) + Short(-1) + Byte(-1) + Int 0 + Byte 0 + Long min（Lucene90NormsConsumer.java:104-138）。VERSION_START=VERSION_CURRENT=0，codec "Lucene90NormsData"/"Lucene90NormsMetadata"（`Lucene90NormsFormat.java:100-105`）。

## 8. Compound 决策

- flush 新段只看 `IndexWriterConfig.useCompoundFile`，默认 **DEFAULT_USE_COMPOUND_FILE_SYSTEM=true**（`IndexWriterConfig.java:99`；`DocumentsWriterPerThread.java:596-607` 直接建 .cfs/.cfe 并 setUseCompoundFile(true)）。noCFSRatio **只影响 merge**：TieredMergePolicy 默认 0.1（`TieredMergePolicy.java:85,100`），MergePolicy.useCompoundFile（`MergePolicy.java:741-759`）。
- **Rust 写非 compound 段完全合法**：等价于 `setUseCompoundFile(false)` 的一等支持路径；.si 里 isCompoundFile 写 -1，SegmentCoreReaders 对非 compound 直接用目录打开各文件（`SegmentCoreReaders.java:104-108`），DirectoryReader/CheckIndex 均透明处理。

## 9. LZ4.java 与 Rust lz4_flex 兼容性

- `LZ4.compress/compressWithDictionary` 输出**标准 LZ4 block 格式裸流**（`util/compress/LZ4.java:510-586`）：token（高 4bit literal len、低 4bit matchLen-4）、0x0F 扩展长度（0xFF 续字节）、literal 字节、**2 字节 LE match offset**、match 长度扩展；以纯 literals 序列结尾（encodeLastLiterals 164-168）。**无 magic、无原长、无 checksum、无 frame**。
- 解压契约：调用方必须已知解压长度，读到产生 `decompressedLen` 字节为止（decompress 87-141）；块间跳转依赖外层 VInt 压缩长度表。
- 兼容判断点（lz4_flex）：必须用 **block 格式**（`lz4_flex::block::compress`，**不能** `compress_prepend_size`，不能 frame 格式）；offset LE、MIN_MATCH=4、token 语义均一致 → 兼容。可以不使用 preset dictionary（不引用 dict 窗口的合法子集），解压端无碍。Lucène 侧参数：FastCompressionHashTable，MEMORY_USAGE=14，hash `(i * -1640531535) >>> (32-hashLog)`，MAX_DISTANCE=65536（LZ4.java:54-68, 300-342）——这些只影响压缩率，不影响格式正确性，Rust 可用任意标准 LZ4 block 压缩器。

## 10. Version 常量汇总（类：常量=值）

- CodecUtil: `CODEC_MAGIC=0x3fd76c17`（:46）、`FOOTER_MAGIC=0xC02893E8`（:49）、footer algorithmID=0、footerLength=16（:421）
- SegmentInfos: codec `"segments"`，`VERSION_70=7 / VERSION_72=8 / VERSION_74=9 / VERSION_86=10 / VERSION_CURRENT=10`（:120-131）
- Lucene99SegmentInfoFormat: `CODEC_NAME="Lucene90SegmentInfo"`，`VERSION_START=VERSION_CURRENT=0`（:83-85）
- Lucene94FieldInfosFormat: `CODEC_NAME="Lucene94FieldInfos"`，`FORMAT_START=0 / FORMAT_PARENT_FIELD=1 / FORMAT_CURRENT=1`（:422-426）
- Lucene90CompressingStoredFieldsWriter: `VERSION_START=VERSION_CURRENT=1`，`META_VERSION_START=0`（:80-82）；fdt codec `"Lucene90StoredFieldsFastData"`（BEST_SPEED）/`"Lucene90StoredFieldsHighData"`（BEST_COMPRESSION）；`INDEX_CODEC_NAME="Lucene90FieldsIndex"`（:68）
- FieldsIndexWriter: `VERSION_START=VERSION_CURRENT=0`（:48-49）
- Lucene90NormsFormat: `VERSION_START=VERSION_CURRENT=0`（:104-105）
- 默认 Codec 名 `"Lucene912"`（Codec.java:58；Lucene912Codec.java:118）；stored fields 参数 chunkSize=81920 / maxDocsPerChunk=1024 / blockShift=10（Lucene90StoredFieldsFormat.java:161,182）
- Version: `LATEST=9.12.3`（Version.java:363）、`MIN_SUPPORTED_MAJOR=8`（:376）
- IndexFileNames: `SEGMENTS="segments"`、`PENDING_SEGMENTS="pending_segments"`（:40-43），generation 用 base36（Character.MAX_RADIX=36）

**无法从源码确认/需实现时注意的点**：
- segments_N 中 `version`（BE Long，索引变更计数）与 `counter`（下一个段名计数）的初始值由 IndexWriter 语义决定（新索引通常 version 从 0/1 起、counter 取已用段数+1），源码未强制具体值，读取端不校验其绝对值。
- diagnostics map 内容（source/lucene version/os 等键）纯调试用途，无读取端校验，Rust 可写最小集。
- userData 可写空 map。
- .fnm 的 dvGen=-1 与 .si 的 isCompoundFile=-1 都依赖 LE 有符号编码，注意 Rust 端按有符号整数处理。
- fdm 里 numDirtyChunks/numDirtyDocs 读取端有一致性校验（numChunks≥numDirtyChunks；dirtyChunks==0 ⟺ dirtyDocs==0，reader 177-190），最后一块强制 flush 的语义要照搬（脏块标记），或保证 flush 总是写 dirty 尾块以使二者一致。