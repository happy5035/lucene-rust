调研完成。以下所有结论均来自本地源码（$SRC = `reference/lucene-9.12.3/lucene/core/src/java/org/apache/lucene`），行号已标注。范围：**仅限 fresh flush（新 segment）的 NUMERIC 与 SORTED 两种 DocValues 写入路径**；读取侧只在"读侧约束决定写入细节"时引用。javadoc 与代码冲突处一律以代码为准，并在第 12 节集中列出。

# Lucene 9.12.3 DocValues 格式调研报告（.dvd/.dvm，NUMERIC + SORTED）

## 1. Codec 标识、版本常量与文件命名

**Lucene90DocValuesFormat**（`codecs/lucene90/Lucene90DocValuesFormat.java`）：

- format 名 `"Lucene90"`（构造函数 :142-144）。
- `DATA_CODEC = "Lucene90DocValuesData"`，`DATA_EXTENSION = "dvd"`；`META_CODEC = "Lucene90DocValuesMetadata"`，`META_EXTENSION = "dvm"`（:158-161）。
- **版本：`VERSION_START = 0`、`VERSION_CURRENT = VERSION_START = 0`（:162-163）。9.12.3 的 Lucene90DocValuesFormat 没有 `VERSION_BIN_COMPRESSED`**——该常量只存在于 backward-codecs 的 Lucene80（`backward-codecs/.../Lucene80DocValuesFormat.java:188`），勿混淆。读侧 `checkIndexHeader(in, codec, 0, 0, id, suffix)`（`Lucene90DocValuesProducer.java:90-97, 113-120`），所以 header 里 version 必须写 0，且 .dvm 与 .dvd 的 version 必须相等（producer :121-124）。
- 类型字节（.dvm 每字段第二个字节，:166-170）：`NUMERIC=0, BINARY=1, SORTED=2, SORTED_SET=3, SORTED_NUMERIC=4`。
- 块常量（:172-183）：
  - `DIRECT_MONOTONIC_BLOCK_SHIFT = 16`（所有 DirectMonotonic 序列统一用 65536 值/块）
  - `NUMERIC_BLOCK_SHIFT = 14`，`NUMERIC_BLOCK_SIZE = 16384`（数值分块分支的块大小）
  - `TERMS_DICT_BLOCK_LZ4_SHIFT = 6` → terms dict 每块 **64** 个 term（SIZE=64, MASK=63）
  - `TERMS_DICT_REVERSE_INDEX_SHIFT = 10` → reverse index 每 **1024** 个 term 抽一条（SIZE=1024, MASK=1023）

**codec 接线**：`Lucene912Codec` 用匿名 `PerFieldDocValuesFormat` 包 `Lucene90DocValuesFormat`（`codecs/lucene912/Lucene912Codec.java:88-94, 122, 192-194, 209-211`），所以**默认 codec 下 DV 文件一定走 PerField 命名**。

**文件命名**：`IndexFileNames.segmentFileName(segmentName, segmentSuffix, ext)`（`index/IndexFileNames.java:90-106`）= `segmentName + "_" + segmentSuffix + "." + ext`。fresh flush 时外层 `SegmentWriteState.segmentSuffix = ""`，PerField 拼出 `segmentSuffix = "Lucene90_0"`（见第 2 节），所以 segment `_0` 的 DV 文件为 **`_0_Lucene90_0.dvd` / `_0_Lucene90_0.dvm`**。`SegmentWriteState` 对 suffix 有断言：要么空、要么恰好两段（下划线分隔）、要么是 base36 generation（`index/SegmentWriteState.java:133-146`），`"Lucene90_0"` 合法。

**两个文件的 header/footer**（`codecs/lucene90/Lucene90DocValuesConsumer.java:66-103`）：构造时立刻写 `CodecUtil.writeIndexHeader(out, codec, 0, segmentInfo.getId(), segmentSuffix)`（.dvd 用 DATA_CODEC，.dvm 用 META_CODEC，:80-95）。header = BE int `0x3fd76c17` + writeString(codec 名) + BE int version + 16 字节 segment ID + 1 字节 suffix 长度 + suffix 字节（`codecs/CodecUtil.java:77-86, 121-135`；suffix = `"Lucene90_0"`，读侧会校验，`Lucene90DocValuesProducer.java:90-97`）。footer = BE int `FOOTER_MAGIC(0xC02893E8)` + BE int 0 + BE long CRC32（对之前全部字节、含 footer 前 8 字节；高 32 位必须 0），共 16 字节（`CodecUtil.java:409-413, 421-423, 643-650`）。整型字节序注意：**header/footer 的 magic/version 是大端**（`writeBEInt`，:653-664），而 `DataOutput.writeInt/writeShort/writeLong` 是**小端**（`store/DataOutput.java:73-89, 223-226`）；VInt/VLong 7 位/字节、高位续传、小组在前，VLong 禁负（:198-204, 236-250）。

## 2. PerFieldDocValuesFormat：suffix 编号、FieldInfo attributes、docValuesGen

`codecs/perfield/PerFieldDocValuesFormat.java`：

- attribute key：`PER_FIELD_FORMAT_KEY = "PerFieldDocValuesFormat.format"`（:62-63），`PER_FIELD_SUFFIX_KEY = "PerFieldDocValuesFormat.suffix"`（:66-67）。per-field 名 `"PerFieldDV40"`（:59）。
- **写 attributes 的时机与位置**：`FieldsWriter.getInstance(FieldInfo, ...)`（:167-238）。每个 DV 字段第一次路过 `addNumericField`/`addSortedField` 时调用：
  - :189 `field.putAttribute(PER_FIELD_FORMAT_KEY, "Lucene90")`；
  - suffix 分配（:209-218）：同一 format 首次为 0，之后每多一个**不同 format** 实例递增；默认 codec 下所有 DV 字段共享一个 Lucene90 实例 → **suffix 恒为 0**；
  - :220-225 `segmentSuffix = getFullSegmentSuffix("", getSuffix("Lucene90", "0")) = "Lucene90_0"`（getSuffix :247-249 = `formatName + "_" + suffix`；getFullSegmentSuffix :251-257），用 `new SegmentWriteState(state, segmentSuffix)`（浅拷贝换 suffix，`SegmentWriteState.java:117-127`）创建真正的 `Lucene90DocValuesConsumer`；
  - :234 `field.putAttribute(PER_FIELD_SUFFIX_KEY, "0")`。
- 调用链：`IndexingChain.writeDocValues` → `DocValuesWriter.flush` → `dvConsumer.addXxxField` → `FieldsWriter.getInstance`。`.fnm` 在 DV 之后写（见第 8 节），所以这两个 attribute 会落进 .fnm（`Lucene94FieldInfosFormat.write` :404 `writeMapOfStrings(fi.attributes())`）。**读侧完全靠这两个 attribute 找文件**（`PerFieldDocValuesFormat.FieldsReader` :288-309：读 format 名 + suffix，缺 suffix 直接 IllegalStateException :295-298）。
- **docValuesGen**：fresh flush 时 `FieldInfo.dvGen` 恒为 **-1**。证据链：`IndexingChain.initializeFieldInfo` 以 `dvGen = -1` 构造 FieldInfo（`index/IndexingChain.java:663-683`，:674 传 -1）；`FieldInfos.Builder.add(fi)` → `add(fi, -1)`（`index/FieldInfos.java:830-831`）；`FieldInfo.setDocValuesGen` 全库唯一调用点是 DV **更新**路径 `ReadersAndUpdates.java:327`（`FieldInfo.java:730-733`）。.fnm 里照写 `writeLong(fi.getDocValuesGen()) = -1`（`Lucene94FieldInfosFormat.java:403`）。**dvGen 不影响 fresh flush 的文件名**——PerField 只在 `dvGen != -1`（更新场景）时才从 attribute 恢复 suffix（`PerFieldDocValuesFormat.java:170, 196-207`）。读侧 `dvGen == -1` 走 base producer（`SegmentDocValuesProducer.java:62-70`）。

## 3. 文件总体结构与 flush 流程

**flush 顺序**（`index/IndexingChain.java`）：

1. `flush()` 内 `writeDocValues(state, sortMap)` 在 norms 之后、points/vectors/stored/postings 之前调用（:261-357，:282）。
2. `writeDocValues`（:402-464）遍历 `fieldHash` 哈希桶（:406-408）——**Java 写 .dvm 的字段顺序是字段名哈希顺序，不是 field number 顺序**。读侧 `readFields` 逐条 `readInt(fieldNumber)` 并 `infos.fieldInfo(fieldNumber)` 查表（`Lucene90DocValuesProducer.java:168-189`），**对顺序无任何要求**；Rust 可按任意顺序（建议 field number 升序）写。
3. 每个有 DV 的字段：`perField.docValuesWriter.flush(state, sortMap, dvConsumer)`（:424）→ `dvConsumer.addNumericField` / `addSortedField`。所有字段共享同一个 `Lucene90DocValuesConsumer`（PerField 内 formats map，`PerFieldDocValuesFormat.java:192-227`），即 **.dvd 数据逐字段顺序追加，.dvm 逐字段顺序追加元数据条目，每条目自带起始 offset/长度**。
4. 全字段写完后 close（:447 → `Lucene90DocValuesConsumer.close` :106-125）：.dvm 末尾 `writeInt(-1)` EOF 标记 + `CodecUtil.writeFooter(meta)`；.dvd 末尾仅 `CodecUtil.writeFooter(data)`。**没有 numFields、没有文件总长等全局字段**（与 postings 的 .psm 不同）。
5. `.fnm` 最后写（`IndexingChain.flush` :347-350，注释 :342-345 明确说明必须在各 consumer flush 之后，以便 consumer 改 FieldInfo——DV 就是靠这个顺序把 attributes 落盘）。

一个 segment 没有任何 DV 字段时根本不创建 dvConsumer（:419-423），无 .dvd/.dvm 文件。segment 有 DV 字段时这两个文件**一定存在且 header 完整**（构造即写）。

## 4. .dvm 逐字段布局

meta 流 = IndexHeader + 逐字段条目 + `int(-1)` + Footer。每个条目：

**公共前缀**：`writeInt(field.number)`（LE int，不是 VInt）+ `writeByte(type)`（NUMERIC=0 / SORTED=2）（`Lucene90DocValuesConsumer.java:130-131, 490-491`）。

**NUMERIC 条目**（= `writeValues(field, producer, ords=false)` 的写入顺序，:186-332；读侧对照 `readNumeric` :197-224）：

| # | 字段 | 编码 | 出处 |
|---|---|---|---|
| 1 | docsWithFieldOffset | LE long | :251/:256/:262 |
| 2 | docsWithFieldLength | LE long | :252/:257/:266 |
| 3 | jumpTableEntryCount | LE short | :253/:258/:267 |
| 4 | denseRankPower | byte | :254/:259/:268 |
| 5 | numValues | LE long | :271 |
| 6 | tableSize | LE int | :277/:286/:303/:312 |
| 7 | table[0..tableSize) | 各 LE long（仅 tableSize>0 时） | :287-289 |
| 8 | numBitsPerValue | byte | :317 |
| 9 | minValue | LE long | :318 |
| 10 | gcd | LE long | :319 |
| 11 | valuesOffset | LE long（.dvd 绝对 fp） | :320-321 |
| 12 | valuesLength | LE long | :329 |
| 13 | valueJumpTableOffset | LE long（无多块跳表时 -1） | :322, 330 |

- 字段 1-4（docsWithField）三种情形（:250-269）：无文档有值 → `(-2, 0, -1, -1)`；所有文档都有值 → `(-1, 0, -1, -1)`；部分有值 → `(offset, length, jumpTableEntryCount, 9)`，数据为 .dvd 里的一段 IndexedDISI（见 5.2）。
- 字段 6-8 的取值组合（:275-315）：
  - `min >= max`（含 numValues=0）：`tableSize=-1, numBitsPerValue=0`；
  - table 压缩：`tableSize=唯一值个数(2..256)` + 升序排好的唯一值表，`numBitsPerValue = unsignedBitsRequired(tableSize-1)`，且 min 改写 0、gcd 改写 1（:294-295）；
  - 分块（doBlocks）：`tableSize = -2 - NUMERIC_BLOCK_SHIFT = -16`，`numBitsPerValue = 0xFF`（即 byte -1）；
  - 普通单块：`tableSize=-1`，`numBitsPerValue = unsignedBitsRequired((max-min)/gcd)`。
  读侧硬约束：`tableSize > 256` 直接 CorruptIndexException（producer :203-206）；`tableSize < -1` → `blockShift = -2 - tableSize`（:213-217）。
- 字段 13：`valueJumpTableOffset` 仅 doBlocks 时为 .dvd 绝对 offset，否则 -1（:322-330）。

**SORTED 条目**（`addSortedField` :488-493 → `doAddSortedField` :495-540）：先按 **ords 的 NUMERIC 条目**写（`writeValues(ords=true)`，共 13 个字段同上），再追加 terms dict 元数据（`addTermsDict` :542-626 + `writeTermsIndex` :646-693；读侧对照 `readTermDict` :278-299）：

| # | 字段 | 编码 | 出处 |
|---|---|---|---|
| 14 | termsDictSize（= numOrds） | VLong | :544 |
| 15 | addresses blockShift | LE int，恒 16（DIRECT_MONOTONIC_BLOCK_SHIFT） | :549 |
| 16 | terms addresses 的 DirectMonotonic meta | 每 DM 块 21 字节：LE long min + LE int floatBits(avgInc) + LE long dataOffset + byte bpv | `DirectMonotonicWriter.flush` :99-111 |
| 17 | maxTermLength | LE int | :614 |
| 18 | maxBlockLength | LE int（最大未压缩块长；无压缩块时 0） | :615-616 |
| 19 | termsDataOffset | LE long | :617 |
| 20 | termsDataLength | LE long | :618 |
| 21 | termsAddressesOffset | LE long | :621 |
| 22 | termsAddressesLength | LE long | :622 |
| 23 | reverse index shift | LE int，恒 10（TERMS_DICT_REVERSE_INDEX_SHIFT） | :648 |
| 24 | index addresses 的 DirectMonotonic meta | 同 #16，共 `1 + ceil(size/1024)` 个 DM 块的记录 | :659-661, 684-685 |
| 25 | termsIndexOffset | LE long | :686 |
| 26 | termsIndexLength | LE long | :687 |
| 27 | termsIndexAddressesOffset | LE long | :690 |
| 28 | termsIndexAddressesLength | LE long | :691 |

注意：

- #16 的 DM meta 记录数 = `numBlocks = ceil(termsDictSize / 64)`（:553-556），读侧由 termsDictSize 自算（producer :281-284），**写侧 numValues 必须恰好等于它**，块数不符会错位解析后面所有字段。空字典（size=0）时 0 块 0 条记录，合法。
- #24 读侧用 `loadMeta(meta, 1 + indexSize, blockShift=16)`（producer :292-294）——注意它复用的是 #15 的 blockShift（16）而非 10，与写侧一致（:659-661 同样用 16）。
- terms addresses / index addresses 的 DM **数据**（packed deltas）不在 .dvm，而是各自 append 到 .dvd（见第 6 节）；DM meta 里的 dataOffset 是相对各自数据区起点的偏移（`DirectMonotonicWriter` 构造时 `baseDataPointer = dataOut.getFilePointer()`，`DirectMonotonicWriter.java:74`，而 terms/index addresses 写的是临时 buffer 再 copy，base 为 0）。

**SORTED_SET/BINARY/SORTED_NUMERIC 不看**；仅记录 .dvm type 字节编号（格式类 :166-170）与 .fnm 的 docValuesByte（`Lucene94FieldInfosFormat.java:243-261`：`NONE=0, NUMERIC=1, BINARY=2, SORTED=3, SORTED_SET=4, SORTED_NUMERIC=5`——两套编号不同，勿混用）。

## 5. NUMERIC 的 .dvd 数据布局

每个 NUMERIC 字段在 .dvd 中至多两段：`[IndexedDISI docsWithField]? [值数据 + 多块跳表]?`。字段间顺序与 .dvm 条目一致。

### 5.1 值的收集与统计（决定编码分支）

`writeValues`（:186-332）先全量扫一遍值（按 docID 升序，每 doc 恰好 1 个值——见第 8 节），统计：

- `numValues`（= 有值文档数）、全局 min/max；
- `gcd`：以 **第一个值** firstValue 为基准，`gcd = gcd(gcd, v - firstValue)`（:199-214）；若任一 `|v| > Long.MAX_VALUE/2` 则放弃 gcd 置 1（:205-211）。`MathUtil.gcd`（`util/MathUtil.java:63-77`）二进制 gcd，输入取绝对值。读侧解码 `value = gcd * packed + minValue`（producer :527-534），gcd 只是压缩手段，任何自洽取值都合法；
- `uniqueValues`：仅当 `ords=false` 时收集，超过 256 个唯一值即放弃（:200, 222-224）；
- 每 16384 个值为一块累计 `spaceInBits`（`MinMaxTracker` :144-184）：全局一份（minMax.finish :231 = 单块编码总代价），按块求和一份（blockMinMax，:218-219, 232）。

### 5.2 docsWithField：三分支（:250-269）

- **全有值**（`numDocsWithValue == maxDoc`，maxDoc = `state.segmentInfo.maxDoc()` :96，**含已删除文档**）：写 meta `(-1, 0, -1, -1)`，.dvd 无数据。读侧按稠密处理（producer :485-537）。
- **全无值**：meta `(-2, 0, -1, -1)`。
- **部分有值**：在 .dvd 当前位置写 IndexedDISI（`IndexedDISI.writeBitSet(values, data, DEFAULT_DENSE_RANK_POWER=9)`，:261-268），meta 记 `(offset, length, jumpTableEntryCount, 9)`。

**IndexedDISI 布局**（`codecs/lucene90/IndexedDISI.java`；写出 :189-254，块 flush :110-133，rank :138-153，跳表 :257-285）：

- 逻辑上按 docID 高 16 位分块（每块 65536 doc，`BLOCK_SIZE = 65536` :102-103）。只为**非空块**落盘，块按 docID 升序：
  - 块头：`writeShort(blockID)` + `writeShort(cardinality - 1)`（LE short，:114-116）；
  - `cardinality ≤ 4095`（`MAX_ARRAY_LENGTH = 4095` :108）→ **SPARSE**：逐 doc 写低 16 位的 LE short，升序（:127-132）；
  - `4096 ≤ cardinality ≤ 65535` → **DENSE**：先写 rank 表（256 字节 = 128 个**大端 u16**，第 k 项 = 前 k*512 bit 之前的置位累计数，:138-153；`DEFAULT_DENSE_RANK_POWER = 9` :106），再写 8192 字节 bitset（1024 个 **LE long**，doc 低 16 位为 bit 序号，`FixedBitSet` 语义：bit i 在 long `i>>6` 的 `1L<<(i&63)`，:123-125）；
  - `cardinality == 65536` → **ALL**：块头之后无数据（:117-118）；
  - **没有自由度**：块类型完全由 cardinality 决定。
- 所有真实块之后写一个**哨兵块**：blockID = `NO_MORE_DOCS>>>16 = 32767`、cardinality=1、SPARSE 条目 65535（:243-251）。
- **块跳表**（jump table）在哨兵块之后：每个逻辑块 8 字节（LE int index + LE int offset）：index = 该块第一个有值文档在"有值文档序列"中的序号（之前块的累计 cardinality），offset = 块头相对 docsWithFieldOffset 的字节偏移；空块条目指向下一个非空块、index 不变（addJumps :257-266）。条目数 = `lastRealBlock + 2`（含哨兵条目）；**若只有 1 个真实块（lastRealBlock=0 → count=2）则不写跳表，meta 里 jumpTableEntryCount=0**（flushBlockJumps :271-285）。写侧返回 short 计数写入 meta 字段 3。
- 读侧依赖：advance 跨 ≥2 块时用跳表（IndexedDISI :466-488），DENSE 块内 advance ≥512 doc 用 rank（:697-717）。CheckIndex 会走这些路径（第 9 节）。

### 5.3 值数据：四种分支

meta 决策顺序（:275-315）：

1. **bpv=0 常量**（`min >= max`）：`numBitsPerValue=0, tableSize=-1`，.dvd 不写任何值数据（valuesLength=0，valueJumpTableOffset=-1）。读侧任意 index 返回 minValue（producer :487-493, 548-554）。numValues=0 时 min=Long.MAX_VALUE、gcd=0 照写（:246-247, 318-319），无实际影响。
2. **table 压缩**（:279-295）：条件 = `uniqueValues != null && size > 1 && unsignedBitsRequired(size-1) < unsignedBitsRequired((max-min)/gcd)`（**严格小于**，bpv 为 round 后的值）。meta：tableSize=size（2..256）+ 升序唯一值表（每值 LE long），bpv=unsignedBitsRequired(size-1)，min=0、gcd=1。.dvd：单块 DirectWriter 流，每值 = 该值在表中的下标（:346-350）。**ords 永不走此分支**（ords=true 时 uniqueValues 为 null，:200）。
3. **分块 doBlocks**（:298-303）：条件 = `minMax.spaceInBits > 0 && blockMinMax.spaceInBits / minMax.spaceInBits <= 0.9`（按块独立 bpv 比全局单块省 ≥10%）。numValues ≤ 16384 时两值相等（比值 1.0）→ **单块字段永不触发**。meta：tableSize=-16、bpv 字节=0xFF、min=全局 min、gcd 照写、valueJumpTableOffset=跳表位置。.dvd 布局（`writeValuesMultipleBlocks` :357-388 + `writeBlock` :390-418）：
   - 每 16384 个值一块（最后一块可不足），值顺序 = docID 顺序。块格式：`byte bpv + LE long blockMin + [bpv>0: LE int packedLength + DirectWriter 数据]`；块内全等（min==max）时只写 `byte 0 + LE long min`。packed 值 = `(v - blockMin) / gcd`（**gcd 用全局那个**，blockMin 按块；packedLength **含** DirectWriter 的尾部 padding，:406-416）。
   - 所有块之后写**跳表**：每块一个 **LE long 绝对 .dvd 文件偏移**（不是相对 valuesOffset！:368-369, 382-385），最后追加一个 `offsetsOrigo`（= 跳表自身起点，:386）。meta.valueJumpTableOffset = offsetsOrigo。读侧 `VaryingBPVReader`（producer :1655-1715）：跳表读块起点 → 顺序解析 `bpv/min/[len]` 头 → DirectReader 读块；解码 `gcd * packed + blockMin`。
4. **普通单块**（:304-313, 325-328, 334-354）：bpv = `unsignedBitsRequired((max-min)/gcd)`；min 归一化：若 `gcd==1 && min>0 && unsignedBitsRequired(max) == unsignedBitsRequired(max-min)` 则 min 改写 0（:305-311，纯空间优化，可不做）。.dvd：一个 DirectWriter 流，numValues 个值，每值 `(v-min)/gcd`（含尾部 padding，计入 valuesLength），valueJumpTableOffset=-1。

DirectWriter 语义见第 7 节。值流顺序 = 迭代顺序 = docID 升序（每 doc 一值）。

## 6. SORTED 的 .dvd 数据布局

`doAddSortedField`（:495-540）：先用第 5 节的数值路径写 **ords**（每 doc 一个 ord，`ords=true`），再 `addTermsDict`（:539）。ords 的硬约束（写侧自检，:234-243）：**min 必须为 0；max>0 时 gcd 必须为 1**（ords 从 0 开始连续——SortedDocValuesWriter 的 ord 覆盖 0..numOrds-1 全部值，见第 8 节）。ords 单块时 `bpv = unsignedBitsRequired(numOrds-1)`；numOrds==1 时走 bpv=0 常量分支；也可能触发 doBlocks。docsWithField 三分支同 NUMERIC。

字段在 .dvd 中的顺序：`[ords 的 IndexedDISI]? [ords 值流+跳表]? [terms dict 数据] [terms addresses DM 数据] [reverse index 数据] [index addresses DM 数据]`。

### 6.1 terms dict（`addTermsDict` :542-626）

term 序列 = ord 0..numOrds-1 的 lookupOrd，即 **unsigned 字节升序**（`BytesRef.compareTo`，`util/BytesRef.java:159-162`；写入侧由 `BytesRefHash.sort()` 保证，`SortedDocValuesWriter.java:113-125` + `util/BytesRefHash.java:146-150` BytesRefComparator.NATURAL）。

- 每 **64** 个 term 一块（TERMS_DICT_BLOCK_LZ4_SHIFT=6，:546-547）。每块开头记一个块地址：`writer.add(data.getFilePointer() - termsDataOffset)`（:578，相对偏移，DirectMonotonic，见 6.2）。
- **块内第一个 term 原样写**：`writeVInt(term.length)` + 原始字节（:581-582），同时拷入压缩缓冲区充当 LZ4 字典（:583-585）。
- **块内第 2..64 个 term 前缀压缩**进缓冲区（:587-600），逐条目：
  - 1 字节 token = `min(prefixLength,15) | (min(15, suffixLength-1) << 4)`（prefix = 与**前一个 term** 的公共前缀长，`StringHelper.bytesDifference`）；
  - `prefixLength >= 15` → 再写 `VInt(prefixLength - 15)`；
  - `suffixLength >= 16` → 再写 `VInt(suffixLength - 16)`；
  - 后缀字节。（读侧解码 producer :1096-1106。）
- 块结束时（下一块开始或全部结束），若缓冲区里除首 term 外还有内容：`writeVInt(uncompressedLength)` + `LZ4.compressWithDictionary(buf, 0, firstTermLen, uncompressedLength, data, ht)`（:571-576, 607-611, 628-635）——即 **VInt 未压缩长度 + 以块首 term 为字典的裸 LZ4 流**（无压缩后长度字段，LZ4 流自终止；解压器按期望长度解码，`LZ4.decompress(DataInput, decompressedLen, dest, dOff)`，`util/compress/LZ4.java:87-89`；读侧 producer :1238-1263）。整块只有 1 个 term 时不写压缩部分（:607）。
- meta：maxTermLength（所有 term 最大长度，:602, 614）、maxBlockLength（各块未压缩长度的最大值，:574, 610, 616）——读侧用它分配解压缓冲区（producer :1080-1083），**必须 ≥ 实际最大值**。
- termsDataOffset/Length = 全部块（含各块 VInt 头）在 .dvd 的范围（:560, 617-618）。

### 6.2 terms addresses（块地址表）

- DirectMonotonic 序列：`numValues = numBlocks = ceil(numOrds/64)`，blockShift=16（:553-556）；值 = 各块起始相对偏移（单调非降——DirectMonotonicWriter.add 强制，`DirectMonotonicWriter.java:124-127`）。
- meta 记录写进 .dvm（第 4 节 #15-16）；packed 数据先写临时 buffer，terms dict 之后 append 到 .dvd（:619-622 记 offset/length）。读侧：randomAccessSlice + DirectMonotonicReader（producer :1066-1069）。

### 6.3 reverse terms index（lookupTerm 用，`writeTermsIndex` :646-693）

- 每 **1024** 个 term 抽一条（ord 0, 1024, 2048, ...）。对每个抽中的 ord：`writer.add(offset)`（当前 index 字节区累计长度），然后写 **sort key**：`ord==0` 写 0 字节；否则写 term 的前 `sortKeyLength = bytesDifference(prevTerm, term) + 1` 字节（prevTerm = ord-1 的 term，:667-677；`StringHelper.sortKeyLength` :62-64）。
- 循环结束再 `writer.add(offset)`（= index 区总长度，:684），共 `1 + ceil(numOrds/1024)` 个 DM 值（:651-654）。
- 落盘顺序：sort key 字节区（termsIndexOffset/Length，:686-687）→ index addresses DM 数据（termsIndexAddressesOffset/Length，:688-691）。DM blockShift 同用 16（:659-661）。
- 读侧仅 `lookupTerm`（seekCeil）走此索引（producer :1131-1160）；`lookupOrd`/迭代不走。**CheckIndex 只用 lookupOrd**（见第 9 节），但结构必须完整合法（长度自洽、DM 可解析、sort key 与地址单调），且真实查询会用它。

## 7. DirectWriter / DirectMonotonic 语义（对照 crates/codec-lucene9/src/packed.rs）

**DirectWriter**（`util/packed/DirectWriter.java`）——DV 值流、DM deltas、norms 等通用，与 postings 的 ForUtil/PForUtil **不是同一套**（postings 是 MSB-first 位流；DirectWriter 是 LSB-first/LE）：

- bpv 白名单：`SUPPORTED_BITS_PER_VALUE = {1,2,4,8,12,16,20,24,28,32,40,48,56,64}`（:225-226），`unsignedBitsRequired` = `max(1, 64-leadingZeros)` 后向上 round 到白名单（:193-200, 221-223；`PackedInts.java:804-806`）。
- 编码（`encode` :101-142）：值按 **LSB 优先**装进小端容器，三种路径——bpv 为 8 的倍数：每值 `bpv/8` 字节 plain LE；bpv ∈ {1,2,4}：每 long 装 `64/bpv` 个值（第 j 个值占 bit `j*bpv`），u64 LE 写；bpv ∈ {12,20,28}：两两合并 `l1 | (l2 << bpv)` 写成 4/8 字节 LE 容器，容器间距 `bpv*2/8` 字节**故意重叠**，整体等价于连续 LSB-first 位流。
- 截断：每批 flush 只写 `ceil(off*bpv/8)` 字节（`PackedInts.Format.PACKED.byteCount`，`PackedInts.java:77-79`；批次为 64 的倍数个值，批边界字节对齐）。
- **尾部 padding**：finish 追加 0-3 个零字节（bpv>32 → (64-bpv)/8 上取整；>16 → (32-bpv)/8；>8 → (16-bpv)/8；≤8 → 0）（:145-174）。**必须写**：读侧 DirectReader 用容器宽度（4/8 字节）定点读，靠 padding 保证最后一次读不越界。
- 对照 `packed.rs`：`direct_writer_encode`（packed.rs:39-104）三分支、重叠容器、截断与 padding 逐点一致（含 12/20/28 重叠回归测试 packed.rs:293-297 注释），与 Java 语义相同。**确认无差异。**

**DirectMonotonicWriter**（`util/packed/DirectMonotonicWriter.java`）：

- 把单调非降序列按 `2^blockShift` 值分块（本格式恒 blockShift=16；允许范围 2..22，:33-34）。块数 = `numValues==0 ? 0 : ((numValues-1)>>>blockShift)+1`（:58），与读侧 Meta 一致（`DirectMonotonicReader.java:56-67`）。
- 每块：avgInc = `(float)((double)(last-first)/max(1,size-1))`；每值先减 `(long)(avgInc*i)`（f32 乘法再截断为 long），再减块内 min 得 delta（:80-97）。**meta 记录（写 meta 流）顺序固定：LE long min → LE int `Float.floatToIntBits(avgInc)` → LE long（数据流 fp − baseDataPointer）→ byte bpv**（:99-111）；delta 用 DirectWriter 写数据流（bpv=0 时不写数据）。
- 读侧还原：`min + (long)(avg*index) + delta`（`DirectMonotonicReader.get` :160-165）。
- 对照 `packed.rs`：`direct_monotonic_write`（packed.rs:113-158）avg 计算、wrapping delta、meta 记录顺序与编码完全一致。**确认无差异。**（packed.rs 的 meta/data 都走 ChecksumIndexOutput；DV terms/index addresses 需把 data 先写内存 buffer 再 append 到 .dvd，语义等价。）

## 8. indexing chain 侧约束（每 doc 单值、字段顺序、.fnm 交互）

- **一个 doc 同一字段只能加一个值**：`NumericDocValuesWriter.addValue` 在 `docID <= lastDocID` 时抛 `"DocValuesField \"X\" appears more than once in this document (only one value is allowed per field)"`（`index/NumericDocValuesWriter.java:51-57`）；`SortedDocValuesWriter.addValue` 同样检查（`index/SortedDocValuesWriter.java:66-72`），另有两条前置校验：value 非 null（:73-76）、**value.length ≤ 32766**（`BYTE_BLOCK_SIZE - 2`，:77-83；`ByteBlockPool.java:43-46`，BYTE_BLOCK_SIZE=1<<15=32768）。NUMERIC 的 null 校验在 `IndexingChain.indexDocValue`（:985-989）。→ fresh flush 每 doc 每字段 0 或 1 个值。
- DV writer 创建：字段第一次出现时按 schema 建（`IndexingChain.initializeFieldInfo` :688-708），dvType != NONE 必有 writer；flush 时 writer 为 null 而 FieldInfo 有 DV 类型是 AssertionError（:426-435）。→ **.fnm 里 dvType != NONE 的字段，.dvm 必有对应条目**（哪怕 0 个文档有值，也写 numDocsWithValue=0 的条目）。
- SortedDocValuesWriter 的 ord：flush 时 `hash.sort()`（unsigned 字节序）生成 termID→ord 映射，ord 覆盖 0..numOrds-1 且**每个 ord 至少被一个 doc 使用**（:113-125）——这正是 writeValues 对 ords 要求 min==0/gcd==1 能被满足的原因。
- DocsWithFieldSet 迭代 = docID 升序（`index/DocsWithFieldSet.java:46-72`），值按加入顺序 → .dvd 值流 = docID 升序。
- .fnm（`Lucene94FieldInfosFormat`，codec 名 `"Lucene94FieldInfos"`，`FORMAT_CURRENT=1`，:419-426）：每字段 `writeString(name), writeVInt(fieldNumber), byte fieldBits, byte indexOptions, byte docValuesByte, writeLong(dvGen=-1), writeMapOfStrings(attributes), ...`（:385-413）。**DV 字段的 attributes 必须含 `PerFieldDocValuesFormat.format="Lucene90"` 与 `PerFieldDocValuesFormat.suffix="0"`**（map 序列化顺序任意，读侧按 key 取）。
- merge/更新路径不在本报告范围（docValuesGen ≥ 1 的 `_gN` 文件、Suffix 从 attribute 恢复等）。

## 9. CheckIndex 校验点（硬性约束清单）

入口 `CheckIndex.testDocValues`（`index/CheckIndex.java:3242-3285`，在 testStoredFields 之后，:1096）：对 FieldInfos 中每个 dvType != NONE 的字段 `checkDocValues`（:3663-3696）。DV reader 用 `getMergeInstance()` 打开（:3251-3254，只影响读缓冲策略，不影响格式约束）。

**NUMERIC**（`checkNumericDocValues` :3639-3661 + `checkDVIterator` :3292-3407）：

- 迭代器初始 docID=-1；nextDoc 序列 docID 严格递增、在 maxDoc 处结束（稠密分支由 maxDoc 截断，IndexedDISI 由哨兵块结束）；
- 抽样做 `advance(doc-1)` 不得回退、`advance` 必找到该 doc（会走 IndexedDISI 跳表路径）、`advanceExact(doc-1)` 的存在性判定必须与上一文档一致且幂等、docID 报告正确；
- `nextDoc` 路径的值必须与 `advanceExact` 路径的值逐点相等（稠密/稀疏、单块/多块、table/gcd 解码一致性）。
- **没有"docsWithField 基数 vs 字段 docCount"校验**——.fnm 不存 DV docCount；CheckIndex 也**不与 stored fields 交叉比对**。

**SORTED**（`checkSortedDocValues` :3434-3496 + checkDVIterator）：

- ord 范围 `[0, getValueCount()-1]`，不得为 -1；
- **maxOrd 必须等于实际使用过的最大 ord**（:3466-3474）；
- **ord 不得有空洞**：`seenOrds.cardinality() == getValueCount()`（:3475-3483）→ numOrds 必须等于字典 term 数且每个 ord 都被引用；
- `lookupOrd(0..maxOrd)` 返回的 term 必须 **unsigned 字节序严格递增**（:3484-3495）；
- advance/advanceExact 一致性同 NUMERIC。

**文件级**（CheckIndex 主流程）：

- `reader.checkIntegrity()`（:1025）→ `CodecReader.checkIntegrity` :305-307 → `PerFieldDocValuesFormat.FieldsReader.checkIntegrity` :354-360 → `Lucene90DocValuesProducer.checkIntegrity` :1644-1646 = **.dvd 全文件 CRC32 校验**（`CodecUtil.checksumEntireFile`）；.dvm 在打开时校验 header + footer CRC（producer :86-106）。→ 两个文件的 footer CRC 必须真实正确。
- 打开期隐含约束：.dvm 每条目的 fieldNumber 必须在 FieldInfos 中存在（producer :170-173）；type 字节 ∈ {0..4}（:185-187）；tableSize ≤ 256（:203-206）；meta/data 两文件 version 相等（:121-124）；header 的 codec 名、segment ID、suffix 与 SegmentReadState 一致；.fnm attributes 指出的 format/suffix 必须能定位到文件（`PerFieldDocValuesFormat.java:288-309`）。

## 10. 常量/版本速查表

| 位置 | 常量 | 值 |
|---|---|---|
| Lucene90DocValuesFormat.java:158-161 | DATA_CODEC / META_CODEC / 扩展名 | "Lucene90DocValuesData" / "Lucene90DocValuesMetadata"；dvd / dvm |
| Lucene90DocValuesFormat.java:162-163 | VERSION_START / VERSION_CURRENT | 0 / 0（无 VERSION_BIN_COMPRESSED） |
| Lucene90DocValuesFormat.java:166-170 | .dvm type 字节 | NUMERIC=0, BINARY=1, SORTED=2, SORTED_SET=3, SORTED_NUMERIC=4 |
| Lucene90DocValuesFormat.java:172-183 | DIRECT_MONOTONIC_BLOCK_SHIFT / NUMERIC_BLOCK_SHIFT(SIZE) / TERMS_DICT_BLOCK_LZ4_SHIFT(SIZE) / TERMS_DICT_REVERSE_INDEX_SHIFT(SIZE) | 16 / 14(16384) / 6(64) / 10(1024) |
| Lucene94FieldInfosFormat.java:243-261 | .fnm docValuesByte | NONE=0, NUMERIC=1, BINARY=2, SORTED=3, SORTED_SET=4, SORTED_NUMERIC=5 |
| IndexedDISI.java:102-108 | BLOCK_SIZE / DENSE_BLOCK_LONGS / DEFAULT_DENSE_RANK_POWER / MAX_ARRAY_LENGTH | 65536 / 1024 / 9 / 4095 |
| DirectWriter.java:225-226 | SUPPORTED_BITS_PER_VALUE | {1,2,4,8,12,16,20,24,28,32,40,48,56,64} |
| DirectMonotonicWriter.java:33-34 | MIN/MAX_BLOCK_SHIFT | 2 / 22（本格式恒用 16） |
| CodecUtil.java:46-49 | CODEC_MAGIC / FOOTER_MAGIC | 0x3fd76c17 / 0xC02893E8 |
| PerFieldDocValuesFormat.java:59-67 | PER_FIELD_NAME / 两个 attribute key | "PerFieldDV40" / "PerFieldDocValuesFormat.format" / ".suffix" |
| SortedDocValuesWriter.java:77-83 | SORTED value 长度上限 | 32766 字节 |
| Lucene912Codec.java:88-94,122 | DV 接线 | PerFieldDocValuesFormat{ Lucene90DocValuesFormat } |

## 11. Rust 实现捷径清单

合法（读侧无差别、CheckIndex 零错误）的简化：

1. **gcd 压缩恒不启用**：NUMERIC 直接以 gcd=1 计算 bpv 并写 gcd=1（绕过 :199-214 的 gcd 统计）。读侧只做 `value = gcd*packed + min` 回乘，无校验。
2. **min 归一化可不做**（:305-311 的 min>0 → 0 改写是纯优化）；table 压缩恒不启用（tableSize=-1）；**doBlocks 恒不启用**（单块 DirectWriter 流 + valueJumpTableOffset=-1）。读侧按 meta 自描述分支解码。
3. **terms dict 的 LZ4 可以只写纯 literal 序列**（token 高 4 位长度、无 match）或不带字典的普通 LZ4 块：解压器预置字典只在遇到 match 时才被引用，纯 literal 流与字典无关。块首 term 的 `VInt+原文` 与块尾 `VInt 未压缩长 + LZ4 流` 的结构必须保留。 crates 里 stored_fields.rs 已用同款论证（stored_fields.rs:290-295）。
4. **DirectMonotonic 的 avgInc 可恒写 0.0f**（每块 min=块最小值、bpv=ubr(maxDelta)），读侧 `min + 0*idx + delta` 完全合法——但 packed.rs 已实现真实 avg，无需走此捷径。
5. **字段顺序任意**：.dvm/.dvd 条目顺序不影响读取（读侧按 fieldNumber 查表）。可按 field number 升序写。
6. **空 DV 字段（0 文档有值）合法**：docsWithField=(-2,0,-1,-1)、numValues=0、bpv=0；SORTED 还可有 termsDictSize=0（DM 0 块、reverse index 1 块全零）。但最小实现可要求调用方不传空字段。
7. **全字段全文档有值时恒 dense**（docsWithFieldOffset=-1）：若 writer 输入保证每 doc 有值，可不实现 IndexedDISI。若有缺失值，IndexedDISI 三分支 + 跳表**必须全真实现**（块类型由 cardinality 强制，无自由度）。

必须全真（无捷径）：

1. **版本号 0、codec 名、segment ID、suffix="Lucene90_0"**（header 逐项校验；meta/data version 相等）。
2. **CRC32 footer**（两文件，CheckIndex 全量校验 .dvd）。
3. **IndexedDISI 结构**（用到时）：块头/SPARSE/DENSE(rank 256B + 8192B bitset)/ALL 阈值、哨兵块、跳表条目语义（含"仅 1 真实块时 entryCount=0"）、denseRankPower=9。
4. **DirectWriter 编码与 padding**（LSB-first/LE、白名单 bpv、ceil(n*bpv/8) 截断 + 0-3 字节 padding）——packed.rs 已一致。
5. **DirectMonotonic meta 记录布局**（min/avgBits/offset/bpv 顺序与编码；块数必须与 numValues 匹配）——packed.rs 已一致。
6. **terms dict 结构**：64 项/块、块首 term 原文、后续前缀压缩条目字节级格式、块地址 DM 表（地址相对 termsDataOffset 且单调）、reverse index（1024 抽样、sort key = 公共前缀+1 字节、1+ceil(n/1024) 个 DM 地址）。LZ4 见捷径 3。
7. **SORTED 的 ords 约束**：min=0、gcd=1（写侧自检强制）；numOrds == termsDictSize；**每个 ord 必须被至少一个 doc 引用**（CheckIndex 空洞检查）；term 严格 unsigned 升序。
8. **meta 数值字段真实性**：numValues、valuesLength、maxTermLength、maxBlockLength（≥实际）、各 offset/length 自洽；tableSize ≤ 256。
9. **.fnm 配套**：DV 字段写 `docValuesByte`（NUMERIC=1/SORTED=3）、`dvGen=-1`、attributes `PerFieldDocValuesFormat.format="Lucene90"` + `suffix="0"`；且 .fnm 中 dvType != NONE 的每个字段都必须在 .dvm 有同类型条目。
10. **VInt/VLong/LE 字节序**与 header/footer 的 BE magic/version 区分。

## 12. javadoc 与代码不一致点（以代码为准）

- `Lucene90DocValuesFormat` javadoc（:77-79）说 table 压缩条件是 "unique values < 256"；代码允许 **≤ 256**（`uniqueValues.size() > 256` 才放弃，:222-224；读侧 tableSize ≤ 256，producer :203-206）。
- javadoc BINARY 段（:102-104）描述的 "Prefix-compressed Binary: chunks of 16" 是旧版 TermsDict 行为；9.12 代码是 **64 项/块 + LZ4（首 term 为字典）+ 1024 抽样 reverse index**（:177-183, 568-605）。javadoc 完全没提 LZ4。
- javadoc NUMERIC 段（:89-91）"blocks of 16384" 与代码一致；但 javadoc 未提 tableSize=-16/bpv=0xFF 的信令，以 writeValues 代码（:298-303）为准。
- javadoc 顶部 DENSE 描述（:49-57，阈值 4096/65535、rank 每 512 doc、u16 表）与 IndexedDISI 代码一致，可作参考。

