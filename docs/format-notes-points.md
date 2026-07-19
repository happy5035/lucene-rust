调研完成。以下所有结论均来自本地源码（$SRC = `reference/lucene-9.12.3/lucene/core/src/java/org/apache/lucene`），行号已标注。范围严格限定 1 维 points：LongPoint（numDims=1, numIndexDims=1, bytesPerDim=8）与 IntPoint（bytesPerDim=4）；读取侧仅在决定写入约束时引用。

---

# Lucene 9.12.3 Points（BKD 树）格式调研报告（.kdm/.kdi/.kdd，仅 1D long/int）

## 0. 通用基础编码

- Codec header/footer、VInt/VLong、`writeInt/writeLong/writeShort` 小端（LE）、`writeBEInt/writeBELong` 大端（BE）等规则与 postings 完全一致，见 `docs/format-notes-postings.md` 第 0 节（`CodecUtil.java:46-49, 77-86, 121-135, 409-413`；`DataOutput.java:73-89, 198-204, 223-250`）。本报告只列 points 特有部分。
- **points 路径完全不使用 ForUtil/PForUtil/DirectWriter/PackedInts 写盘**。叶子里的 "packed values" 只是公共前缀裁剪后的原始字节串联，没有任何 bit-packing（见第 5 节）。

## 1. codec 标识、版本与文件命名

**三个文件与 codec 名**（`codecs/lucene90/Lucene90PointsFormat.java:48-59`）：

| 文件 | 扩展名 | IndexHeader codec 名 |
|---|---|---|
| data（叶块） | `.kdd`（`DATA_EXTENSION`，:53） | `"Lucene90PointsFormatData"`（:48） |
| index（内部节点 packed index） | `.kdi`（:56） | `"Lucene90PointsFormatIndex"`（:49） |
| meta（per-field 元数据） | `.kdm`（:59） | `"Lucene90PointsFormatMeta"`（:50） |

- `VERSION_START = 0`，`VERSION_CURRENT = VERSION_START = 0`（:61-62）。9.12.3 写 0，读侧 `checkIndexHeader` 接受区间 [0,0]（`Lucene90PointsReader.java:63-93`），**没有任何版本分支**。
- 三文件都在构造时用 `CodecUtil.writeIndexHeader`（含 16 字节 segment ID + 1 字节 suffix 长度，suffix 恒为空串即写 `0x00`）（`Lucene90PointsWriter.java:59-98`）。
- **文件名无 per-field suffix**：`IndexFileNames.segmentFileName(segmentName, segmentSuffix, ext)`（`IndexFileNames.java:90-106`），points 的 `writeState.segmentSuffix` 恒为 `""`（`DocumentsWriterPerThread.java:400-407` 构造 `SegmentWriteState` 不传 suffix），所以文件名就是 `_N.kdd/_N.kdi/_N.kdm`。
- **PointsFormat 不经 PerField 包装**（与 postings 的 `PerFieldPostingsFormat`、DV 不同）：`Lucene912Codec.pointsFormat()` 直接 `return new Lucene90PointsFormat()`（`Lucene912Codec.java:162-163`，对比同文件 :79/:137-138 postings 是 PerField 包装）；flush 时直接 `state.segmentInfo.getCodec().pointsFormat()`（`index/IndexingChain.java:372`）。因此 `.fnm` attributes **不需要任何 points 相关条目**（postings 需要在 attributes 里记录 per-field codec/suffix，points 没有这回事）。

**.kdm 内还有第二层 header**：每个 field 的 meta 条目以 `CodecUtil.writeHeader(metaOut, "BKD", 9)` 开头（`util/bkd/BKDWriter.java:1244`；`CODEC_NAME="BKD"` :82）。BKD 自有版本号：`VERSION_START=4`、`VERSION_LEAF_STORES_BOUNDS=5`、`VERSION_SELECTIVE_INDEXING=6`、`VERSION_LOW_CARDINALITY_LEAVES=7`、`VERSION_META_FILE=9`、`VERSION_CURRENT=9`（`BKDWriter.java:83-89`）。9.12.3 恒写 9；读侧接受 [4,9]，version≥6 才读 numIndexDims、≥9 才从 meta 读 dataStartFP/indexStartFP（`util/bkd/BKDReader.java:56-113`）。写侧无分支。

**FieldInfo 的 point 三字段**：由 `IndexingChain.PerField.setPoints(dimensionCount, indexDimensionCount, numBytes)` 在首次见到该字段的 point 值时记录（`IndexingChain.java:1505-1513`，调用点 :827-831），flush 前构造 `FieldInfo` 时传入（:663-683，参数 :676-678）。LongPoint/IntPoint 的 `FieldType`：`type.setDimensions(numDims, Long.BYTES/Integer.BYTES)`（`document/LongPoint.java:50-55`、`document/IntPoint.java:48-53`），2 参 `setDimensions(d, n)` = 3 参 `setDimensions(d, d, n)`（`document/FieldType.java:292-295`），**即 numIndexDims == numDims == 1**，bytesPerDim = 8/4。`.fnm` 中每 field 依次写：attributes map（`codecs/lucene94/Lucene94FieldInfosFormat.java:404`）、`VInt pointDimensionCount`，非 0 时再写 `VInt pointIndexDimensionCount`、`VInt pointNumBytes`（:405-409）。LongPoint 字段 = (1, 1, 8)，IntPoint = (1, 1, 4)，attributes 为空 map。

## 2. flush 流程与三文件整体布局

**调用链**（全部 `index/IndexingChain.java`）：`flush` → `writePoints(state, sortMap)`（:289，位于 writeDocValues 之后、vectors 之前）→ 逐 field `perField.pointValuesWriter.flush(state, sortMap, pointsWriter)`（:381）→ 全部 field 写完后 `pointsWriter.finish()`（:389）、`close()`（:394）。

- `index/PointValuesWriter.java:92-228`：把内存缓冲的 (packedValue, docID) 包成匿名 `MutablePointTree`（:95-161），经一个临时 `PointsReader` 调 `Lucene90PointsWriter.writeField(fieldInfo, reader)`（:227）。
- `Lucene90PointsWriter.writeField`（`codecs/lucene90/Lucene90PointsWriter.java:120-176`）：values 是 `MutablePointTree` → `BKDWriter.writeField(...)`（:140-148），拿到 `IORunnable finalizer` 后 **先 `metaOut.writeInt(fieldInfo.number)`（LE int32），再执行 finalizer 写该 field 的 meta**（:145-146）。finalizer 为 null（0 个点）时该 field 不写任何 .kdm 条目。
- `Lucene90PointsWriter.finish()`（:281-292）：`metaOut.writeInt(-1)` → `CodecUtil.writeFooter(indexOut)`、`writeFooter(dataOut)` → `metaOut.writeLong(indexOut.getFilePointer())`（LE，.kdi 全长含 footer）→ `metaOut.writeLong(dataOut.getFilePointer())`（.kdd 全长含 footer）→ `writeFooter(metaOut)`。
- 读侧对账（`Lucene90PointsReader.java:95-116`）：循环 readInt 直到 -1；读两个长度后用 `CodecUtil.retrieveChecksum(in, expectedLength)` 精确校验 .kdi/.kdd 全长与 CRC。

**多 field 布局**：`.kdd` 与 `.kdi` 都是逐 field 追加——每个 field 的叶块从该 field 的 `dataStartFP` 开始紧密相连，packed index 从 `indexStartFP` 开始；`.kdm` 逐 field 记录这两个 fp（见第 6 节）。**一个 segment 内同一 field 只写一个 BKD 树**（IndexingChain 每 field 只 flush 一次，:361-384）。`.kdm` 条目顺序 = IndexingChain 遍历 field 的顺序，读侧按 fieldNumber 建 map，无顺序假设（`Lucene90PointsReader.java:38, 95-104`）。

**每个 field 的写出时序**：先把全部叶块写进 `.kdd`（writeField/add 阶段），再一次性写 `.kdm` 条目 + 把 packed index 追加到 `.kdi`（finalizer 阶段，`BKDWriter.writeIndex`，见第 6 节）。

## 3. BKDConfig

`util/bkd/BKDConfig.java`：
- `DEFAULT_MAX_POINTS_IN_LEAF_NODE = 512`（:26）；`MAX_DIMS = 16`（:29，注意 `BKDWriter` javadoc :69 说 "1 to 8" 已过时）；`MAX_INDEX_DIMS = 8`（:32）。
- 派生量：`packedIndexBytesLength = numIndexDims * bytesPerDim`、`packedBytesLength = numDims * bytesPerDim`、`bytesPerDoc = packedBytesLength + 4`（+docID int，:65-68）。1D long：8/8/12；1D int：4/4/8。
- 校验（:71-103）：numDims∈[1,16]、numIndexDims∈[1,8]、numIndexDims≤numDims、bytesPerDim>0、maxPointsInLeafNode∈(0, ArrayUtil.MAX_ARRAY_LENGTH]。
- `maxPointsSortInHeap`：不是 BKDConfig 字段，在 `BKDWriter` 构造时算：`maxMBSortInHeap(默认 DEFAULT_MAX_MB_SORT_IN_HEAP = 16.0f，BKDWriter.java:95) * 1MB / bytesPerDoc`（`BKDWriter.java:168`），并要求 ≥ maxPointsInLeafNode（:171-181）。long：16MiB/12 = 1398101 点；int：/8 = 2097152 点。**它只决定 add/finish 通用路径用 HeapPointWriter 还是 OfflinePointWriter（`BKDWriter.java:195-205`），对 1D 正常 flush 路径和磁盘格式毫无影响**（见第 4 节）。

## 4. 1D 写入主路径：排序与叶划分

**1D flush 不走 add/finish 通用路径**，而是 `BKDWriter.writeField` → `writeField1Dim`（`BKDWriter.java:435-436, 565-597`）：

1. `MutablePointTreeReaderUtils.sort(config, maxDoc, reader, 0, size)`（:572；`MutablePointTreeReaderUtils.java:40-85`）：`StableMSBRadixSorter` 按 **packedBytes 无符号字节序**整体排序；若原序列 docID 非降则直接稳定排序（不再加 tie-break 键），否则在排序键尾部追加 docID 的 bitsRequired(maxDoc-1) 位作 tie-break（:43-57, 76-83）。**净效果：叶序列全局按 (value 升序, docID 升序) 排**。`PointValues.IntersectVisitor.visit(docID, packedValue)` 的 javadoc 也明确承诺 1D 这个顺序（`index/PointValues.java:313-317`）。
2. `OneDimensionBKDWriter`（`BKDWriter.java:651-829`）顺序消费排好序的点流：
   - 每攒满 `maxPointsInLeafNode`（512）个**立即写一叶**（:720-727）；`finish` 时把不足 512 的尾巴写成最后一叶（:733-737）。**叶大小固定 512，仅最右叶可为余数**。`numLeaves = ceil(pointCount / 512)`。注释（:721-723）称 "N>1 维每叶 max/2..max"，与 9.12.3 实际代码不符——所有路径都是"前 numLeaves-1 叶恰好 512 + 最右叶余数"（通用路径 `build` 的 `mid = from + numLeftLeafNodes * maxPointsInLeafNode`，:1732/:1984，同理）。
   - `leafBlockFPs[i]` = 第 i 叶在 .kdd 的起始 fp（:795）；`leafBlockStartValues` = **除第一叶外每叶首值**（:790-794）→ 即内部节点 split 值来源（split 值 = 右子树最左叶的首值）。
   - 全局 `minPackedValue` = 第一叶首值、`maxPackedValue` = 最后一叶末值（:778-786）；`docsSeen` FixedBitSet 累计 distinct docID（:708）。
   - `leafCardinality` = 叶内不同值的个数（:696-700），用于 values 块分支选择。
3. 结束返回 finalizer：`writeIndex(metaOut, indexOut, 512, leafNodes, dataStartFP)`（:771-773）。

**HeapPointWriter / OfflinePointWriter**（`BKDWriter.java:195-205`；`util/bkd/HeapPointWriter.java`、`util/bkd/OfflinePointWriter.java`）：只在 `add()` + `finish()` 通用路径使用（N 维 flush、1D 但 reader 非 MutablePointTree 的 merge 兜底）。内存排序缓冲，记录布局 = packedValue + docID（Heap 用 BE int 存 docID，`HeapPointWriter.java:75`；Offline 写 `Integer.reverseBytes` 即 BE，`OfflinePointWriter.java:66-68`，便于按字节比较）。超过 maxPointsSortInHeap 时 Offline spill 到临时文件（带 footer，:110-119），由 `BKDRadixSelector` 做多路 radix select/sort（`BKDRadixSelector.java:96-134, 484-545`；排序键 = split dim 值 + 其余 data dims + docID，:71-74）。**这些纯是写侧内存策略：1D flush 主路径完全不经过它们，且读侧只关心最终叶序列。** merge 时 1D 走 `BKDWriter.merge`（`BKDWriter.java:604-649`，`BKDMergeQueue` 按 (value, docID) 归并已排序段，:380-394），同样进 `OneDimensionBKDWriter`。→ **Rust 恒用"全内存 (value, docID) 升序排序 + OneDimensionBKDWriter 语义"产出完全合法、且与 Java flush 字节一致的文件。**

## 5. .kdd 叶子块布局

每个叶块（从 `leafBlockFPs[i]` 开始）依次四段（写侧 `OneDimensionBKDWriter.writeLeafBlock`，`BKDWriter.java:776-828`；读侧 `BKDReader.BKDPointTree.readDocIDs` :633-642 + `visitDocValuesWithCardinality` :786-851）：

```
VInt count                                ; 本叶点数（512 或最右叶余数）   BKDWriter.java:1266-1271
docs 块（DocIdsWriter，见 5.1）            ; :803 → :1270
commonPrefix 段（1D 恰一组）               ; :804 → writeCommonPrefixes :1475-1482
  VInt commonPrefixLen                    ; 1D 下 = 叶内首值与末值的公共前缀长（已排序 ⇒ 即全叶公共前缀）:799-801
  byte prefix[commonPrefixLen]            ; 取自叶内首值
values 块（三分支，见 5.2）                ; :826 → writeLeafBlockPackedValues :1273-1320
```

### 5.1 DocIdsWriter 分支全集（`util/bkd/DocIdsWriter.java:60-146`）

先扫描本叶 count 个 docID：求 min/max、是否**严格递增**（strictlySorted，:63-76，`min2max = max-min+1`）。标志字节 + 分支：

| flag (byte) | 名称 | 触发条件（短路顺序） | 布局 |
|---|---|---|---|
| `-2` | CONTINUOUS_IDS | strictlySorted 且 min2max == count（:77-82） | `writeVInt(docIds[0])`，隐含 count 个连续 ID |
| `-1` | BITSET_IDS | strictlySorted 且 min2max ≤ count<<4（:83-91） | `writeVInt(offsetWords=min>>>6)` + `writeVInt(totalWordCount=FixedBitSet.bits2words(max-(offsetWords<<6)+1))` + totalWordCount 个 `writeLong`(LE)：第 w 个 long 的 bit b 置位 ⇔ doc = offsetWords*64 + w*64 + b 存在（:148-179） |
| `16` | DELTA_BPV_16 | min2max ≤ 0xFFFF（:94-109） | `writeVInt(min)`；`halfLen=count>>>1`；写 halfLen 个 `writeInt`(LE)：第 i 个 int = `((doc[i]-min)<<16) \| (doc[halfLen+i]-min)`；count 为奇数时末尾再 `writeShort`(LE) 最后一个 delta |
| `24` | BPV_24 | max ≤ 0xFFFFFF（:111-138） | 每 8 个 doc 打包成 3 个 `writeLong`(LE)：8×24bit 按 MSB-first 串联成 192 位（l1 = doc1≪40\|doc2≪16\|doc3≫8，即 l1 高 24 位是 doc1……）分 3 个 u64 各 LE 写 8 字节（:124-133）；不足 8 个的尾巴每 doc 写 `writeShort(doc>>>8)` + `writeByte(doc)`（:135-138） |
| `32` | BPV_32 | 其余（:139-144） | count 个 `writeInt`(LE) 原样 |

- `LEGACY_DELTA_VINT = 0`（:36）只读不写（:200-202）。
- **写侧分支选择是自由的**：读侧按 flag 分派（:182-206）。Rust 最小实现可恒用 BPV_32（或先判连续/稀疏用更省的前两支）。注意 BITSET/DELTA16 只在 strictlySorted 时才可能命中；叶内同值多 doc 时 docs 非严格递增（允许相等），会落到 16/24/32 分支。

### 5.2 values 块三分支（`BKDWriter.writeLeafBlockPackedValues`，:1273-1320）

1 字节 `compressedDim` 开头（读侧 `readCompressedDim` 校验 ∈ {-2,-1}∪[0,numDims)，`BKDReader.java:937-945`）：

- **`-1` 全等叶**：`prefixLenSum == packedBytesLength`（1D 即 commonPrefixLen == bytesPerDim）时只写 `writeByte(-1)`，无后续字节（:1281-1284）。读侧把整个 commonPrefix 当唯一值（`BKDReader.java:799-803, 893-901`）。
- **`-2` 低基数**：当 `lowCardinalityCost ≤ highCardinalityCost`（:1287-1318；count==cardinality 时恒选高基数：low=1 > high=0）。布局 = 逐 run：`writeVInt(runLen)` + 该值的 suffix（`bytesPerDim - commonPrefixLen` 字节），run 为"同一值连续重复段"，各 run 合计必须 == count（写 `writeLowCardinalityLeafBlockPackedValues` :1322-1358；读 `visitSparseRawDocValues` `BKDReader.java:866-890` 校验合计）。
- **`0` 高基数**（1D 的 sortedDim 恒 0）：`writeByte(0)`，然后按"commonPrefixLen 处那一字节"做 run-length（`writeHighCardinalityLeafBlockPackedValues` :1360-1384）：每 run 写 `writeByte(该字节)` + `writeByte(runLen)`（≤0xff，超长切多段，`runLen()` :1460-1473）+ run 内每点的剩余 suffix（`bytesPerDim - commonPrefixLen - 1` 字节，`writeLeafBlockPackedValuesRange` :1441-1458）。读侧把 commonPrefix + run 字节 + suffix 拼回原值（`BKDReader.java:903-935`，合计校验 :931-934）。

**1D 不写 actual bounds**：`if (config.numIndexDims != 1) writeActualBounds(...)`（:1325-1327, 1368-1370）——1D 叶块里没有任何 min/max 附加字段，叶子边界完全由 packed index 的 split 链推出（读侧对应 `BKDReader.java:804` 的 else 分支）。

分支代价模型（:1295-1310）：`highCardinalityCost = count*(packedBytesLength - prefixLenSum - 1) + 2*numRunLens`；`lowCardinalityCost = cardinality*(packedBytesLength - prefixLenSum + 1)`。**分支选择是写侧自由**（读侧三分支都支持）：Rust 可"全等 → -1，其余恒高基数"。

## 6. .kdi packed index 与 .kdm per-field meta

### 6.1 树形（写读两侧必须一致）

- nodeID 按 BFS 从 1 编；叶子 = nodeID ≥ numLeaves 的节点；`isLeafNode()`（`BKDReader.java:481-483`）。左→右叶序对应 `leafBlockFPs[0..numLeaves-1]`。
- 左右子树叶数划分固定算法 `getNumLeftLeafNodes(numLeaves)`（`BKDWriter.java:831-847`）：`lastFullLevel = floor(log2(numLeaves))`，`leavesFullLevel = 1<<lastFullLevel`，`numLeft = leavesFullLevel/2 + min(numLeaves - leavesFullLevel, leavesFullLevel/2)`。**这是结构契约**：读侧 `size()`/`estimatePointCount` 依赖同一划分与"每叶 512、最右叶余数"假设（`BKDReader.java:298-300, 519-521`；`rightMostLeafNode = 2^(floor(log2(numLeaves))+1) - 1`，:297）。
- 1D split：dim 恒 0（`OneDimensionBKDWriter` 的 `getSplitDimension` 恒返回 0，:762-764）；split 值 = `leafBlockStartValues[splitOffset]`，其中 `splitOffset = leavesOffset + numLeftLeafNodes - 1`（:1100-1105），即**右子树最左叶的首值**（中位切分，左子树全 ≤ split ≤ 右子树首值）。

### 6.2 packed index 字节布局（`BKDWriter.packIndex`/`recursePackIndex`，:1019-1223）

前序递归编码。`lastSplitValues` 初始全 0（:1025）、`negativeDeltas` 初始全 false（:1034）。root 以 `isLeft=false, minBlockFP=0, leavesOffset=0, numLeaves` 调用（:1028-1037）。

```
innerNode := [仅当本节点是父的右孩子: VLong(leftMostLeafFP - 父leftMostLeafFP)]   ; :1087-1097
             VInt(code)                                                          ; :1136-1144
             byte suffix[suffixLen-1]   (suffixLen = bytesPerDim - prefix；仅当 suffixLen>1) ; :1147-1151
             左子树编码
             [仅当左子是内部节点: VInt(leftNumBytes)]                              ; :1189-1198（先占位后回填）
             右子树编码
leafNode  := [仅当右孩子: VLong(leafFP - 父leftMostLeafFP)]；左孩子为空（0 字节）   ; :1074-1084
```

- `code = (firstDiffByteDelta * (1 + bytesPerDim) + prefix) * numIndexDims + splitDim`（:1137-1138）。1D 简化为 `firstDiffByteDelta*(bytesPerDim+1) + prefix`。
- `prefix` = 本节点 split 值与 **lastSplitValues[dim]**（该维最近祖先 split 值，初始全 0）的公共前缀长；`firstDiffByteDelta` = 两值在 prefix 处字节的无符号差，若 `negativeDeltas[dim]`（上次沿该维向左递归，:1174-1175）则取负值（存其相反数）；随后 `lastSplitValues[dim]` 更新为本 split 值供子节点压前缀，递归返回后恢复（:1153-1164, 1213-1217）。
- suffix 字节 = split 值 `prefix+1 .. bytesPerDim-1` 的原始字节（第一个差分字节已由 delta 隐含）。
- fp 语义：左孩子继承父的 leftMostLeafFP（不落盘）；右孩子落 VLong delta。右子树递归时传的 minBlockFP 仍是父 leftBlockFP（:1205）。读侧镜像：`leafBlockFPStack[level] = leafBlockFPStack[level-1] (+ readVLong 若右孩子)`（`BKDReader.java:657-662`）。
- **root 的首个字段是 VLong(第一个叶块 fp)（相对 0，即绝对 fp）**。特例 `numLeaves == 1`：整个 packed index 就是这一个 VLong（:1074-1084 的 isLeft=false 叶分支）。
- `leftNumBytes` 让读侧跳过左子树直接定位右孩子（`rightNodePositions`，`BKDReader.java:708-715`）。
- 容量约束：`bytesPerDim * numLeaves ≤ ArrayUtil.MAX_ARRAY_LENGTH`（`checkMaxLeafNodeCount`，:871-878）。

### 6.3 .kdm per-field meta 逐字段（`BKDWriter.writeIndex`，:1236-1264；读侧 `BKDReader` 构造 :56-113）

`.kdm` 文件体 = IndexHeader("Lucene90PointsFormatMeta", 0, segID, "") 之后，逐 field：

```
int32(LE) fieldNumber                          ; Lucene90PointsWriter.java:145/:172
Header("BKD", 9)                               ; BE magic 0x3fd76c17 + VInt(3)+"BKD" + BE int32 9（CodecUtil.writeHeader）:1244
VInt numDims (=1)                              ; :1245
VInt numIndexDims (=1)                         ; :1246
VInt maxPointsInLeafNode (=512)                ; :1247
VInt bytesPerDim (=8/4)                        ; :1248
VInt numLeaves                                 ; :1251
byte minPackedValue[packedIndexBytesLength]    ; 1D = bytesPerDim 字节 :1252
byte maxPackedValue[packedIndexBytesLength]    ; :1253
VLong pointCount                               ; 总点数（含同 doc 多值、含重复值）:1255
VInt docCount                                  ; docsSeen.cardinality()，distinct docID 数 :1256
VInt packedIndexByteLength                     ; :1257
int64(LE) dataStartFP                          ; 该 field 首叶块在 .kdd 的 fp :1258
int64(LE) indexStartFP                         ; packedIndex 在 .kdi 的 fp :1261
```

随后 `.kdi` 追加 `packedIndex` 原始字节（:1263）。全部 field 之后：`int32(LE) -1` + `int64(LE) indexFileLength` + `int64(LE) dataFileLength` + Footer（`Lucene90PointsWriter.java:286-291`）。读侧校验：BKD header version∈[4,9]、min≤max 逐维（`BKDReader.java:82-95`）、用 packedIndexByteLength+indexStartFP slice 出 .kdi 段（:160-171）。`dataStartFP` 读出为 `minLeafBlockFP` 字段但 9.12 读侧不再使用（:44, 102）——仍必须写正确值。

## 7. 值的字节序

- LongPoint：`NumericUtils.longToSortableBytes`（`util/NumericUtils.java:210-214`）：`value ^= 0x8000000000000000L` 后按 **big-endian 8 字节**写入（`BitUtil.VH_BE_LONG`）。IntPoint：`intToSortableBytes`（:187-191）：`value ^= 0x80000000` 后 BE 4 字节。接入点：`document/LongPoint.java:172-174`、`document/IntPoint.java:151-153`。
- 效果：packed 后的无符号字节序 == 原数值的有符号序。BKD 内所有比较（排序、min/max、split、CheckIndex）都是**无符号字节数组比较**（`ArrayUtil.getUnsignedComparator(bytesPerDim)`）。
- 叶子 values 块的 suffix 就是这些 sortable bytes 的子串，**与 DirectWriter/PackedInts 完全无关**；docs 块的 16/24/32 分支用 LE 原语（见 5.1），这是两种字节序并存的点，别混淆。

## 8. docCount vs pointCount、多值与缺失语义

- doc 缺该字段 ⇒ 无任何点（`PointValuesWriter.addPackedValue` 只在字段存在时调用，`IndexingChain.java:765-766`）。
- 一个 doc 多个值 ⇒ 多个点（每个值一条 (value, docID)，不去重，`PointValuesWriter.java:51-81` 的 TODO 注释明确不 dedup）。
- meta 里 `pointCount` = 点总数（VLong），`docCount` = distinct docID 数（VInt，`docsSeen.cardinality()`，`BKDWriter.java:1255-1256, 240/708`）。读侧 `PointValues.size()`/`getDocCount()` 直接返回二者（`BKDReader.java:991-998`）。
- 0 点的 field：`.kdm` 无该 field 条目（finalizer 为 null，`Lucene90PointsWriter.java:144-148, 171-174`；读侧 `getValues` 返回 null，CheckIndex 跳过，`CheckIndex.java:2606-2609`）。

## 9. CheckIndex 对 points 的硬性约束清单

`index/CheckIndex.java`：`testPoints`（:2588-2688）+ `VerifyPointsVisitor`（:2874-…），配合 `PointValues.intersect` 前序遍历（`PointValues.java:346-381`，每节点先 `compare(min,max)` 再下钻/访问叶子）与 `estimatePointCount`（:387-442，cross 叶按 `(size+1)/2` 估、inside 按 `size()` 全计）：

1. **遍历有序性（1D 专属硬约束）**：`intersect` 全程 value 单调不减；同值时 docID 单调不减（允许同 doc 同值重复）（:3027-3058）。⇒ 叶从左到右值升序、叶内 (value, docID) 升序。
2. **每点 ∈ 所属 cell**：visit 时校验值在最近一次 `compare` 报告的 [cellMin, cellMax] 内（:2986-3025）。1D 叶子的 cell 边界完全由 split 链推出（根 = meta min/max；左子 [min, split]，右子 [split, max]，`BKDReader.pushBoundsLeft/Right` :375-426）。⇒ **split 值必须满足 左子树所有值 ≤ split ≤ 右子树所有值**；取"右子树最左叶首值"天然满足。
3. **cell 合法性**：`compare` 校验 min ≤ max，且 cell ⊆ [globalMin, globalMax]（meta 的 min/maxPackedValue 必须是真实全局最值）（:3061-3135）。
4. **计数对账**：`docCount ≤ size`（:2908-2916）、`docCount ≤ maxDoc`（:2918-2926）、遍历实际点数 == size（:2642-2650）、遍历 distinct docID 数 == docCount（:2652-2660）。
5. **estimatePointCount**：全 cross ≥ size/2、全 inside ≥ size、全 outside == 0（:2616-2636）。⇒ **只有最右叶可不满 512**（读侧 `size()` 按 `pointCount % 512` 推最右叶大小，`BKDReader.java:298-300, 519-521`），且树形划分必须同 `getNumLeftLeafNodes`。
6. min/maxPackedValue 非空、长度 == packedIndexBytesLength（:2928-2961）；meta min ≤ max 逐维（`BKDReader.java:82-95`）。
7. 叶块编码合法性：`compressedDim ∈ {-2,-1}∪[0,numDims)`（`BKDReader.java:937-945`）；低基数/高基数分支各 run 合计必须 == count（:886-889, 931-934）；docs 块 flag 必须 ∈ 已知集合（`DocIdsWriter.java:203-205`）。
8. 文件级：三文件 IndexHeader codec 名/版本(0)/segmentID/suffix 匹配，footer CRC；`.kdm` 结尾两长度与 `.kdi/.kdd` 实际全长（含 footer）一致（`Lucene90PointsReader.java:63-116`）；`checkIntegrity` 对 .kdi/.kdd 全量 CRC（:144-147）。

## 10. Rust 实现捷径清单（最小但完全合法的 1D BKD writer）

1. **恒全内存排序**：把所有 (sortableValue, docID) 按无符号字节序（等价数值升序）+ docID 升序一次排完，直接等价 Java 1D flush 主路径（`writeField1Dim`）。不需要 HeapPointWriter/OfflinePointWriter/BKDRadixSelector/临时文件——它们对 1D 磁盘字节零影响。
2. **叶划分固定**：排序流顺序切 512/叶；`numLeaves = ceil(n/512)`；`leafBlockFPs[i]` 按写盘顺序记。这是硬约束（读侧 size/estimate 假设），无自由度。
3. **split 策略**：无自由度需求也无自由度风险——内部节点划分用 `getNumLeftLeafNodes`，split 值取右子树最左叶首值（即第 splitOffset+1 叶首值），splitDim=0。读侧容忍任何满足"左≤split≤右"的切分，但按 Java 语义做可同时满足 CheckIndex 与字节一致。
4. **packed index 编码无捷径**：必须逐字节复刻 `recursePackIndex`（fp-delta 链、prefix 压缩、negative-delta 规则、leftNumBytes 回填、root 开头 VLong 绝对 fp、numLeaves==1 时仅一个 VLong）。这是 .kdi 的唯一合法编码。
5. **DocIdsWriter 可恒用 BPV_32**（`writeByte(32)` + count 个 LE int32）；想省空间再加 CONTINUOUS_IDS(-2)/BITSET_IDS(-1)/DELTA_BPV_16。全部分支读侧合法。
6. **values 块可恒用高基数分支**：全等叶（commonPrefixLen==bytesPerDim）必须写 `-1`；其余 `writeByte(0)` + 按 commonPrefixLen 处字节 run-length（run ≤255）+ suffix。低基数 -2 可不实现。
7. **commonPrefixLen 合法区间 [0, 全叶真实公共前缀]**；写 0 也合法（体积略大），写"首末值公共前缀"（已排序 ⇒ 等于全叶前缀）与 Java 字节一致。写超过真实前缀会损坏数据。
8. **字节序硬点**：codec header/footer 的 magic/version 是 BE；`writeInt/writeLong/writeShort` 是 LE；VInt/VLong 是 7-bit 小端组序；排序/比较一律无符号字节；LongPoint/IntPoint 值 = 符号位翻转后 BE 8/4 字节。
9. **meta 字段硬点**：`pointCount` 含重复、`docCount` 必须 distinct；`maxPointsInLeafNode` 必须写 512（读侧据此算最右叶大小）；`dataStartFP`/`indexStartFP` 必须与实际 fp 一致；`.kdm` 末尾两长度 = `.kdi`/`.kdd` 含 footer 全长。
10. **必须字节级一致的点**：三文件 IndexHeader（codec 名、version 0、segment ID、空 suffix）与 footer CRC；"BKD"/9 内层 header；meta 字段顺序与类型（LE int32 fieldNumber、VInt/VLong/LE int64）；叶块四段顺序；packed index 编码。
11. **可跳过的情况**：0 点 field 不写 .kdm 条目；merge 路径（1D 归并）与 flush 路径产出同构，Rust 只需实现 flush 语义。
