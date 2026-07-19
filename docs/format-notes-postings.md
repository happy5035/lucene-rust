调研完成。以下所有结论均来自本地源码（$SRC = `reference/lucene-9.12.3/lucene/core/src/java/org/apache/lucene`），行号已标注。

---

# Lucene 9.12.3 倒排索引格式调研报告（.tim/.tip/.doc）

## 0. 通用基础编码

- **Codec header/footer**（`CodecUtil`）：
  - `CODEC_MAGIC = 0x3fd76c17`（`CodecUtil.java:46`），`FOOTER_MAGIC = ~CODEC_MAGIC = 0xC02893E8`（:49）。
  - `writeHeader`：BE int magic + `writeString`(VInt 长度 + UTF-8) + BE int version（:77-86）。`writeIndexHeader` 追加 16 字节 segment ID + 1 字节 suffix 长度 + suffix 字节（:121-135）。
  - `writeFooter`：BE int FOOTER_MAGIC + BE int 0（algorithmID=zlib-crc32）+ BE long CRC32（对之前所有字节、含 footer 前 8 字节；高 32 位必须为 0）（:409-413, 643-650）。footer 总长 16 字节（:421-423）。
  - 注意：header/footer 的 int/long 是**大端**（`writeBEInt`/`writeBELong`，:653-664），而 `DataOutput.writeInt/writeShort/writeLong` 是**小端**（`DataOutput.java:73-89, 223-226`）。
- **VInt/VLong**：7 位/字节、高位续传标志、小端组序（`DataOutput.java:198-204, 236-250`）。VLong 不允许负值。`writeZLong` = zigzag + VLong（:259-261；`BitUtil.zigZagEncode(long) = (l>>63)^(l<<1)`，`BitUtil.java:293`）。
- **GroupVInt**（`util/GroupVIntUtil.java`）：4 个 u32 一组：1 字节 flag（每值 2 bit 表示 字节数-1，从高到低对应第 1..4 个值，:145）+ 各值小端定长（1/2/3/4 字节）。不足 4 个的尾部用普通 VInt（:159-162）。`MAX_LENGTH_PER_GROUP = 17`（:30）。

## 1. .doc 文件布局

**Codec 名称与版本**（`Lucene912PostingsFormat.java:354-361`）：
- `DOC_CODEC = "Lucene912PostingsWriterDoc"`，`META_CODEC = "Lucene912PostingsWriterMeta"`（.psm 文件），`POS_CODEC = "Lucene912PostingsWriterPos"`，`PAY_CODEC = "Lucene912PostingsWriterPay"`，`TERMS_CODEC = "Lucene90PostingsWriterTerms"`。
- `VERSION_START = 0`，`VERSION_CURRENT = 0`。
- `BLOCK_SIZE = ForUtil.BLOCK_SIZE = 128`（:342；`ForUtil.java:32`）。

**相对旧版（Lucene90/PreLucene912）的变化**：9.12 把 skip data 重构为只有两层（每 128 docs 的 level-0 与每 4096 docs 的 level-1）并**内联在 postings 流中**（每个 packed block 前面），旧版 skip data 集中放在每个 term postings 的末尾且多层（package-info.java 历史章节 :414-417；格式描述 `Lucene912PostingsFormat.java:59-62`）。

**整体布局**（javadoc :160-164）：
```
.doc --> IndexHeader("Lucene912PostingsWriterDoc", v0), <TermFreqs>^TermCount, Footer
TermFreqs --> <PackedBlock32>^(floor(docFreq/4096)), <PackedBlock>*余下整块, VIntBlock?
```
（TermFreqs 间无分隔，靠 .tim 的 docStartFP + docFreq 定位。）

**PackedBlock32（level-1 组）**：`Level1SkipData, <PackedBlock>^32`。触发条件：每写满 32 个 packed block（即 docCount % 4096 == 0）时，把 level-1 skip data **前置**写入（`Lucene912PostingsWriter.flushDocBlock:433-436`）。docFreq < 4096 的 term 完全没有 level-1 头（reader 侧 `reset()` 里 `docFreq < LEVEL1_NUM_DOCS` 直接顺序读，`Lucene912PostingsReader.java:433-441`）。

**Level1SkipData 逐字段**（writer `writeLevel1SkipData:444-486`；reader `Lucene912PostingsReader.java:860-897` 与 :535-544）：
- `writeVInt(docID - level1LastDocID)`（上一个 level-1 边界最后 doc，首个为 -1）
- 若有 freqs：`writeVLong(level1Len)`，其中 `level1Len = 2*2 + scratch.size() + level1Output.size()`，含义 = 从该 VLong 之后到本组末尾（32 个 block 结束）的总字节数；随后 `writeShort(scratch.size()+2)`（LE，= impacts 字节数+2+pos/pay skip 数据）、`writeShort(numImpactBytes)`（LE）、impacts 字节、（若有 positions：`writeVLong(posFPDelta)` + `writeByte(posBufferUpto)`；若再有 offsets/payloads：`writeVLong(payFPDelta)` + `writeVInt(payloadByteUpto)`）；然后 32 个 PackedBlock 的内容。
- 若无 freqs（IndexOptions.DOCS）：`writeVInt(docDelta)` + `writeVLong(level1Output.size())` + 32 个 block，无 impacts。

**PackedBlock（level-0，128 docs）**：`Level0SkipData, PackedDocDeltaBlock, PackedFreqBlock?`（writer `flushDocBlock:375-432`；reader `skipLevel0To:548-571`、`moveToNextLevel0Block:919-931`）：
1. `writeVLong(skip0NumBytes)` = 其后 skip 字段总字节数（= VInt15 长度 + VLong15 长度 + impacts/pos/pay skip 字段长度）。读侧 `skip0EndFP = fp + skip0NumBytes` 即数据块起点。
2. `writeVInt15(docID - level0LastDocID)`（本 block 最后 doc 与上一 block 最后 doc 的差，首个为 -1）。`VInt15`（writer :357-373；reader `readVInt15:2043-2050`）：若 `v <= 0x7FFF` 写 2 字节 LE short；否则写 `short(0x8000|(v&0x7FFF))` + `writeVLong(v>>>15)`。
3. `writeVLong15(blockLength)` = 从该字段之后到本 block 结束（skip 字段 + doc delta 块 + freq 块）的字节数。
4. 若有 freqs：`writeVLong(impactLength)` + impacts 字节 +（有 positions：`writeVLong(posFPDelta)` + `writeByte(posBufferUpto)`；有 offsets/payloads：再 `writeVLong(payFPDelta)` + `writeVInt(payloadByteUpto)`）。
5. **PackedDocDeltaBlock**：`ForDeltaUtil` 编码的 128 个 doc gap（第一个 gap 是相对 prevDocID=-1 的 delta，即 docID+1）。**不用 patching**。
6. **PackedFreqBlock**（有 freqs 时）：`PForUtil` 编码的 128 个 freq，**用 patching**。

doc delta 块在 freq 块之前；skip data 在最前。

**VInt tail（不足 128）**（`PostingsUtil.writeVIntBlock:55-72`）：剩余 `n = docFreq % 128` 个：每个值预处理为 `(docDelta << 1) | (freq==1 ? 1 : 0)`（无 freqs 时就是纯 docDelta），用 **GroupVInt** 写出；然后对每个 `freq != 1` 的值追加一个普通 VInt freq。**tail 前没有 skip0NumBytes 等任何 skip 字段**（reader `refillRemainder:501-520` 直接读）。tail 的 doc/freq 交织在同一流里，这点与 packed block 不同。

**singleton 优化（docFreq==1）**：整个 term 不写 .doc 数据（`finishTerm:518-525`），`singletonDocID = docDeltaBuffer[0]-1` 内联进 .tim 元数据；freq 隐式等于 totalTermFreq。positions（若有）仍写 .pos。与 totalTermFreq 无关，只要 docFreq==1 即触发。

**skip data 触发条件汇总**：level-0 = 每个满 128 的 packed block 前都有；level-1 = 每满 32 个 block（4096 docs）在该组前有；tail 与 singleton 无 skip data。

**impacts**（`CompetitiveImpactAccumulator`）：level-0 每 block 累计 (freq, norm)（无 norms 时 norm=1），level-1 为 32 个 block 的 addAll 合并。`getCompetitiveFreqNormPairs()`（:103-124）：norm 按无符号升序遍历，保留 maxFreq 严格递增的 (freq,norm) 对；norm 超出 [-128,127] 走 TreeSet 逻辑。写盘（`writeImpacts:488-504`）：首对 prev=(0,0)，`freqDelta = freq-prevFreq-1`，`normDelta = norm-prevNorm-1`；normDelta==0 写 `VInt(freqDelta<<1)`，否则 `VInt((freqDelta<<1)|1) + writeZLong(normDelta)`。CheckIndex 会用 ImpactsEnum 校验 impacts，所以必须写真实竞争对。

**.psm meta 文件**（`Lucene912PostingsFormat.META_EXTENSION = "psm"`，:324）：IndexHeader("Lucene912PostingsWriterMeta", v0)，close 时写 4 个 **LE int**：`maxNumImpactsAtLevel0, maxImpactNumBytesAtLevel0, maxNumImpactsAtLevel1, maxImpactNumBytesAtLevel1`，再 `writeLong`(LE) `.doc` 文件全长（**含 16 字节 footer**，因为 writeFooter 之后取 getFilePointer），若有 .pos 再 writeLong 其全长，再有 .pay 同理，最后 footer（`Lucene912PostingsWriter.close:660-671`；reader 用 `retrieveChecksum(in, expectedLength)` 精确校验全长，`Lucene912PostingsReader.java:100-116, 152`）。

## 2. ForUtil / PForUtil / ForDeltaUtil 算法要点

**ForUtil**（128 个值的 bit-packing，`ForUtil.java`）：
- 输入先按 primitive 压缩：bpv<=8 → collapse8（8 值/long，16 longs）；bpv<=16 → collapse16（4 值/long，32 longs）；否则 collapse32（2 值/long，64 longs）（:120-133）。
- 然后把这些 longs 的 primitive 槽位按 MSB 优先连续重排成 `bpv*2` 个 long（`numLongsPerShift = bpv*2`，:139），每个 long 以 **`DataOutput.writeLong`（小端 8 字节）** 写盘（:189-191）。总字节数 `numBytes(bpv) = bpv << 4 = bpv*16`（:195-197）。
- Rust 重写等价做法：把 128 个值按 bpv 位宽 MSB-first 串联成一个 `128*bpv` 位的比特流（即标准的 PackedInts 语义：值 0 占最高位），按 64 位分组，**每组 u64 小端**写 8 字节。decode 端无对齐要求（任意 fp 起读），块长恒为 bpv*16 字节。

**ForDeltaUtil**（doc deltas，无 patching）：先写 1 字节 `bitsPerValue`（:248-273）：
- 若全部 128 个 delta 都为 1 → 写字节 `0`，无后续数据。
- 否则 bpv = bitsRequired(128 个 delta 的按位或)，写字节 bpv，再按 ForUtil 编码，但 primitive 阈值不同：**bpv<=4 → collapse8；bpv<=11 → collapse16；否则 collapse32**（:261-269）。
- decode 时做前缀和还原 docID。

**PForUtil**（freqs / positions / payload lengths 等，有 patching，`PForUtil.java`）：
- 找 top-8（`MAX_EXCEPTIONS = 7`，堆大小 8，:30, 60-69）。`maxBitsRequired = bitsRequired(最大值)`；`patchedBitsRequired = max(bitsRequired(第 8 大值), maxBitsRequired - 8)`（:76-79，patch 只能降 8 位，因为异常高字节存 1 字节）。
- numExceptions = top-8 中 `> (1<<patchedBitsRequired)-1` 的个数；这些值被掩码降位，异常记录为 2 字节 `[index byte, 高 bits 字节]`（:87-99）。
- 写盘：若降位后全部相等且 `maxBitsRequired <= 8` → 写 1 字节 `(numExceptions<<5)` + `writeVLong(公共值)`；否则写 1 字节 token `(numExceptions<<5) | patchedBitsRequired` + `ForUtil.encode(值, patchedBitsRequired)`。最后写 `2*numExceptions` 字节异常表（:101-113）。allEqual 情况下异常字节预先左移 patchedBitsRequired 位（:102-105）。
- decode：token 低 5 位 bpv、高 3 位 numExceptions；bpv==0 → VLong 填充；然后 `longs[idx] |= exceptionByte << bpv`（:117-130）。
- 注意 encode 会**原地修改**输入数组（掩码降位），Rust 里别复用。

## 3. .tim / .tmd 文件布局

**Codec 名称与版本**（`Lucene90BlockTreeTermsReader.java:78-104`）：
- `.tim`：`TERMS_CODEC_NAME = "BlockTreeTermsDict"`；`.tip`：`"BlockTreeTermsIndex"`；`.tmd`：`"BlockTreeTermsMeta"`。
- `VERSION_START = 0`、`VERSION_MSB_VLONG_OUTPUT = 1`、`VERSION_FST_CONTINUOUS_ARCS = 2`、`VERSION_CURRENT = 2`（:83-94）。Lucene912 写出 version=2。

**.tim**：IndexHeader("BlockTreeTermsDict", 2)，然后逐 field 的 NodeBlock 序列（block 按写入顺序，非深度优先），Footer。**注意**：PostingsHeader（`"Lucene90PostingsWriterTerms"` v0 + `writeVInt(BLOCK_SIZE=128)`）实际写在 **.tmd** 里（`Lucene912PostingsWriter.init:209-213`，被 blocktree 用 metaOut 调用，`Lucene90BlockTreeTermsWriter.java:339`；reader 对应 `Lucene90BlockTreeTermsReader.java:173` + `Lucene912PostingsReader.init:188-206`）——`Lucene912PostingsFormat` javadoc 里"PostingsHeader 在 .tim per-field"的描述是过时的。

**block 大小**：`DEFAULT_MIN_BLOCK_SIZE = 25`、`DEFAULT_MAX_BLOCK_SIZE = 48`（`Lucene90BlockTreeTermsWriter.java:223, 229`）；约束 min>=2、max>=min、max >= 2*(min-1)（:352-370）。分块逻辑：逐 term 入 pending 栈，当前缀关闭且栈顶共享该前缀的条目数 >= 25 时写 block（`pushTerm:1105-1146`）；若某前缀下条目 > 48 则切成多个 **floor block**（贪心：每满 25 条且剩余 > 48 就切，`writeBlocks:714-741`）。

**NodeBlock 磁盘布局**（`writeBlock:801-1059`，reader `SegmentTermsEnumFrame.loadBlock:145-240`）：
1. `writeVInt(code)`，`code = (numEntries << 1) | isLastInFloor`（该 floor 组最后一个 block 或单 block 时置 1，:828-834）。
2. suffix 字节块：`writeVLong(token)`，`token = (numSuffixBytes << 3) | (isLeafBlock ? 0x04 : 0) | compressionCode`（:1011-1016）；`compressionCode`：0=NO_COMPRESSION，1=LOWERCASE_ASCII，2=LZ4（`CompressionAlgorithm.java:25-47`）。压缩触发条件（:985-1009）：`suffixBytes > 2*numEntries && prefixLength > 2` 才考虑；先试 LZ4（平均 suffix > 6 字节且 HighCompressionHashTable 压缩后节省 > 25% 才用），否则试 LowercaseAsciiCompression。**Rust 可恒用 NO_COMPRESSION，读侧按 flag 解码即可**。
3. suffix 长度流：`writeVInt((numSuffixLengthBytes << 1) | allEqualFlag)`；若全等再写 1 字节公共值，否则写全部字节（:1026-1037）。逐条目的编码：
   - leaf block（无子 block）：`VInt(suffixLength)`（:880）。
   - 非 leaf：term → `VInt(suffixLength << 1)`（:918）；子 block → `VInt((suffixLength << 1) | 1)` + `writeVLong(startFP - subBlock.fp)`（回退指针，:945-965）；**子 block 条目不写 stats 和 metadata**。
4. stats 块：`writeVInt(numStatsBytes)` + stats 字节（:1040-1043）。stats 流（`StatsWriter:601-631`；reader `decodeMetaData:454-472`）：
   - singleton run（`docFreq==1 && (!hasFreqs || totalTermFreq==1)`）：遇到非 singleton 或块结束时写 `VInt(((runLength-1) << 1) | 1)`。
   - 普通：`VInt(docFreq << 1)`；若 hasFreqs 再 `writeVLong(totalTermFreq - docFreq)`。
5. metadata 块：`writeVInt(numMetaBytes)` + 每 term 的 postings metadata（见第 6 节）。每个 block 的**第一条** metadata 用 `absolute=true`（即从 0 起算的 delta），其余相对前一条（:855, 888-889；reader `decodeMetaData:440` 按位置推断，磁盘上无标志位）。

**floor block 机制**：一个前缀下条目过多时切成多个 floor block；第一个 block 的 FST 输出（见第 4 节）额外携带后续 block 的定位表。父 block 中的子 block 条目、FST 输出中的 `hasTerms` 标志（该 block 是否含 term）用于读时跳过。

**.tmd（FieldMetadata）**：IndexHeader("BlockTreeTermsMeta", 2) + PostingsHeader（见上）+ `writeVInt(numFields)`，然后逐 field（`TermsWriter.finish:1174-1188`；reader `Lucene90BlockTreeTermsReader.java:175-238`）：
1. `VInt fieldNumber`
2. `VLong numTerms`
3. `VInt rootCode.length` + rootCode 字节（root block 的 FST 输出，见第 4 节）
4. 若 indexOptions != DOCS：`VLong sumTotalTermFreq`（DOCS 字段此值不写出，reader 把第一个 VLong 同时当作两者）
5. `VLong sumDocFreq`
6. `VInt docCount`（含该 field 至少一个 posting 的文档数）
7. minTerm：`VInt len` + bytes；8. maxTerm 同（:1184-1185）
9. `VLong indexStartFP`（该 field FST 在 .tip 中的起始 fp）
10. FST metadata（`FST.FSTMetadata.save`，`FST.java:1220-1259`）：`CodecUtil.writeHeader(metaOut, "FST", 9)` + 1 字节 emptyOutput 标志（=1 时：`VInt len` + **逆序**的 emptyOutput 序列化字节）+ 1 字节 inputType（BYTE1=0）+ `VLong startNode` + `VLong numBytes`。
- 全部 field 之后：`writeLong(indexLength)` + `writeLong(termsLength)`（LE，均为含 footer 的文件全长）+ footer（`close:1230-1238`；reader :243-258 校验）。

DOCS 字段的 `totalTermFreq = -1`（`PushPostingsWriterBase.writeTerm:172`），`sumTotalTermFreq` 不写、stats 中也不写 ttf。

## 4. .tip 与 FST

**.tip 文件**：IndexHeader("BlockTreeTermsIndex", 2)，逐 field 一段 FST 原始字节（起点 = 该 field 的 indexStartFP，长度 = metadata.numBytes），Footer。

**FST 类型**：`FST<BytesRef>`，`INPUT_TYPE.BYTE1`，`ByteSequenceOutputs`（输入 = block 前缀字节 → 输出 = 字节串；**不是** PositiveIntOutputs）。输出序列化 = `VInt length + bytes`（`ByteSequenceOutputs.java:115-119`；finalOutput 同）。输出内容（`compileIndex:490-521`；reader 侧 `FieldReader.readVLongOutput:112-118`）：
- 前导值用 **MSB 顺序 VLong**（`writeMSBVLong:447-458`：按 7 位组**大端在前**写出，每字节除最后一字节外置 0x80；因 blocktree version=2 >= VERSION_MSB_VLONG_OUTPUT=1）编码 `encodeOutput(fp, hasTerms, isFloor) = (fp << 2) | (hasTerms ? 2 : 0) | (isFloor ? 1 : 0)`（:408-413；flag 位 `OUTPUT_FLAG_IS_FLOOR=0x1`、`OUTPUT_FLAG_HAS_TERMS=0x2`，`Lucene90BlockTreeTermsReader.java:72-75`）。
- 若 isFloor：`writeVInt(blocks.size()-1)`，然后对每个后续 floor block（普通 LSB VLong/VInt）：`writeByte(floorLeadByte)` + `writeVLong((sub.fp - fp) << 1 | (sub.hasTerms ? 1 : 0))`（:509-520）。
- root block 的输出就是 .tmd 里的 rootCode（它也是 FST 的 emptyOutput，因 root 前缀为空串，`finish:1166-1169`）。

**FST 序列化要点**（`FST.java`）：
- metadata 见上节。version：`VERSION_START=6`、`VERSION_LITTLE_ENDIAN=8`（=`VERSION_90`）、`VERSION_CONTINUOUS_ARCS=9`=`VERSION_CURRENT`（:111-128）；blocktree v2 下用 9（`Lucene90BlockTreeTermsWriter.java:536-540`）。
- 字节流结构：**第 0 字节恒为 0x00 padding**（构造器预计数 `numBytesWritten++`，`FSTCompiler.java:173-174`；地址 0 保留为 NON_FINAL_END_NODE，FINAL_END_NODE=-1 永不落盘），节点地址 = 该节点最后一个字节的偏移；读取走 reverse reader（从后往前）。
- 非定长节点：scratch 中按 label 升序逐 arc 写 `[flags:1][label:1][output?][finalOutput?][target VLong?]`（:439-499），flags：
  - `BIT_FINAL_ARC=0x01`（arc 可终止）、`BIT_LAST_ARC=0x02`（节点最后一条 arc）、`BIT_TARGET_NEXT=0x04`（target 是上一个冻结节点时省略 target 字节，仅非定长节点使用，:450-455）、`BIT_STOP_NODE=0x08`（target 无出弧，即 target<=0）、`BIT_ARC_HAS_OUTPUT=0x10`、`BIT_ARC_HAS_FINAL_OUTPUT=0x20`（`FST.java:78-88`）。
  - target VLong 仅当 `target.node > 0 && !BIT_TARGET_NEXT` 时写。
  - 整个节点 scratch **整体逆序**后追加到输出流（`reverseScratchBytes:700-709`），返回地址 = `numBytesWritten - 1`（:563-566）。VInt/VLong 随节点一起被逆序，reader 反向重组。**Rust 实现只需正向序列化再整体 reverse**。
- 定长（fixed-array）节点：当 `(depth <= 3 && numArcs >= 5) || numArcs >= 10`（`FIXED_LENGTH_ARC_SHALLOW_DEPTH=3 / SHALLOW_NUM_ARCS=5 / DEEP_NUM_ARCS=10`，`FSTCompiler.java:76-86, 600-605`）：
  - 节点头（伪 arc）：`[nodeFlags][VInt numArcs/labelRange][VInt bytesPerArc]`，位于 scratch 起始（逆序后处于节点末尾，读取时先遇到）。
  - `ARCS_FOR_BINARY_SEARCH = 0x20`（= BIT_ARC_HAS_FINAL_OUTPUT 值复用）：每条 arc 补齐到 `maxBytesPerArc`（含 label+flags）（:651-695）。
  - `ARCS_FOR_DIRECT_ADDRESSING = 0x40`：头中写 **labelRange** 与 `maxBytesPerArcWithoutLabel`；随后 `ceil(labelRange/8)` 字节 presence bits（bit k 表示 label = firstLabel+k，字节内 LSB 先，`writePresenceBits:790-811`）、first label、各 arc 去 label 后补齐（:727-788）。选择条件 `shouldExpandNodeWithDirectAddressing`（:616-649，默认 oversizing factor=1.0 + credit 机制）。
  - `ARCS_FOR_CONTINUOUS = 0x60`（label 连续且 version>=9）：同 direct addressing 但无 presence bits（:539-545, 769-772）。
  - 用户提到的 `BIT_ARCS_PER_BYTE` 是旧文档概念，9.12 源码中已被上述 fixed-array 三种形式取代。
- 空输出：`NO_OUTPUT` = 空 BytesRef（0 字节），不落盘（靠 flag 区分有无 output）；blocktree 输出永非空。
- **FSTCompiler 增量构建注意点**（Rust 重写关键）：
  - 输入必须**全局升序**（按 unsigned 字节序）；blocktree 对它 `suffixRAMLimitMB(0)` → `dedupHash = null` → **不做节点去重/后缀共享**（`FSTCompiler.java:180-183`；`Lucene90BlockTreeTermsWriter.java:542-546`），这大幅简化实现：每个 frontier 节点只编译一次，`lastFrozenNode` 即上一个编译的节点地址（决定 BIT_TARGET_NEXT）。
  - `add()` 流程（:849-944）：与上一输入求公共前缀，`freezeTail(prefixLenPlus1)` 冻结不再变化的尾部节点；沿公共前缀把 output 前推（`outputs.common/subtract` = 字节串公共前缀拆分，ByteSequenceOutputs:43-113）；新输入剩余弧挂到 frontier。空输入只允许第一个 add，直接设 emptyOutput（:860-868）。
  - `compile()`：`freezeTail(0)` 后编译 root，`startNode = root 地址`；`numBytes` 含 padding 字节（:998-1018, 954-968）。若 FST 只接受空串则 startNode=0。
  - Java reader 对结构无版本耦合校验（flag 自描述），Rust 若求稳可先只写非定长节点（合法且可读），再逐步对齐 fixed-array 与 continuous 优化；但若追求与 Java 字节级一致，需完整复刻选择启发式（含 credit）。

## 5. IndexOptions = DOCS_AND_FREQS 的差异

- `.pos` / `.pay` 文件是否创建取决于**整个 segment**：`state.fieldInfos.hasProx()` 为 false 时两个文件都不创建；`hasPayloads()||hasOffsets()` 为 false 时不创建 .pay（`Lucene912PostingsWriter:148-189`）。若 segment 中无 position 字段，.psm 中也不写 pos/pay 文件长度（:665-670）。
- .tim 元数据（`encodeTerm:631-641`）：DOCS_AND_FREQS 只写 docFPDelta/singletonDocID 一项；`posStartFP/payStartFP/lastPosBlockOffset` **完全不写**（`posStartFP` 保持为 0/前值但不落盘）。
- singleton 行为相同（docFreq==1 即内联，freq 隐式 = totalTermFreq）。skip data：level-0/level-1 中无 posFPDelta/posBufferUpto/payFPDelta/payByteUpto 字段，impacts 照常（norm 缺失时按 1 计）。
- stats：hasFreqs=true → 非 singleton term 写 `VLong(ttf - df)`；.tmd 写 sumTotalTermFreq。

## 6. IntBlockTermState 在 .tim metadata 中的字段与编码顺序

字段（`Lucene912PostingsFormat.IntBlockTermState:425-457`）：`docStartFP, posStartFP, payStartFP, lastPosBlockOffset, singletonDocID`（docFreq/totalTermFreq 在 stats 流，不在此）。

编码（`Lucene912PostingsWriter.encodeTerm:604-643`；reader `decodeTerm:235-277`）：
1. `VLong l`：
   - 通常 `l = (docStartFP - prevDocStartFP) << 1`（block 首条 absolute 时 prev=0）；若 `singletonDocID != -1` 再写 `VInt(singletonDocID)`。
   - 特例（前一条与本条都是 singleton 且 docStartFP 相同）：`l = (zigZagEncode(singletonDocID - prevSingletonDocID) << 1) | 1`，不再写 VInt（:614-629）。
2. 若有 positions：`VLong(posStartFP delta)`；若再有 payloads/offsets：`VLong(payStartFP delta)`。
3. 若有 positions 且 `totalTermFreq > BLOCK_SIZE`（此时 `lastPosBlockOffset = 最后一个 packed pos block 结束处相对 posStartFP 的偏移`）：`VLong(lastPosBlockOffset)`；否则不落盘（TermState 中记 -1）（:533-538）。

## 7. Version 常量清单

| 类：位置 | 常量 | 值 |
|---|---|---|
| Lucene912PostingsFormat.java:360-361 | VERSION_START / VERSION_CURRENT | 0 / 0 |
| Lucene912PostingsFormat.java:342 | BLOCK_SIZE | 128 |
| Lucene912PostingsFormat.java:347-352 | LEVEL1_FACTOR / LEVEL1_NUM_DOCS / LEVEL1_MASK | 32 / 4096 / 4095 |
| Lucene90BlockTreeTermsReader.java:83-94 | VERSION_START / VERSION_MSB_VLONG_OUTPUT / VERSION_FST_CONTINUOUS_ARCS / VERSION_CURRENT | 0 / 1 / 2 / 2 |
| Lucene90BlockTreeTermsWriter.java:223,229 | DEFAULT_MIN_BLOCK_SIZE / DEFAULT_MAX_BLOCK_SIZE | 25 / 48 |
| FST.java:114-128 | VERSION_START / VERSION_LITTLE_ENDIAN(=VERSION_90) / VERSION_CONTINUOUS_ARCS / VERSION_CURRENT | 6 / 8 / 9 / 9 |
| CodecUtil.java:46-49 | CODEC_MAGIC / FOOTER_MAGIC | 0x3fd76c17 / 0xC02893E8 |
| ForUtil.java:32 | BLOCK_SIZE | 128 |
| PForUtil.java:30 | MAX_EXCEPTIONS | 7 |
| FSTCompiler.java:76-86 | FIXED_LENGTH_ARC_SHALLOW_DEPTH / SHALLOW_NUM_ARCS / DEEP_NUM_ARCS | 3 / 5 / 10 |
| GroupVIntUtil.java:30 | MAX_LENGTH_PER_GROUP | 17 |
| Codec 名 | .doc="Lucene912PostingsWriterDoc" v0；.psm="Lucene912PostingsWriterMeta" v0；.pos="Lucene912PostingsWriterPos" v0；.pay="Lucene912PostingsWriterPay" v0；.tmd 内 PostingsHeader="Lucene90PostingsWriterTerms" v0+VInt(128)；.tim="BlockTreeTermsDict" v2；.tip="BlockTreeTermsIndex" v2；.tmd="BlockTreeTermsMeta" v2；FST meta="FST" v9 | |

**未能确认/需要注意的点**：
- 全部关键点均已在本地源码确认，无需网络。两个小提醒：(a) `Lucene912PostingsFormat` javadoc 的格式描述（如 PostingsHeader 在 .tim、PackedBlock32 结构示意）与代码存在出入，以代码为准（本报告均按代码）；(b) `BIT_ARCS_PER_BYTE` 在 9.12 源码中已不存在，对应物是 fixed-array arcs 三种 nodeFlags。
- Rust 实现的兼容捷径（读侧自描述，Java 9.12.3 可正常读并通过 CheckIndex）：suffix 恒用 NO_COMPRESSION；FST 可只写非定长节点；但 impacts 必须按 `CompetitiveImpactAccumulator` 语义写真实值，.psm 中的 4 个 max 统计必须与实际写出的 impacts 一致，文件长度记录必须含 16 字节 footer。
## 8. 实测补充（Java 9.12.3 实写索引 + 源码确认）

- postings 文件名带 per-field 后缀：`_0_Lucene912_0.{doc,psm,tim,tip,tmd}`。来源：flush 时 SegmentWriteState.segmentSuffix=""（DocumentsWriterPerThread.java:400-407），DefaultIndexingChain 用 PerFieldPostingsFormat 包裹 codec 的 postings format，后者给每个 format 分配后缀 `formatName + "_" + suffixId`（PerFieldPostingsFormat.java:123-124, getSuffix），首个 format → "Lucene912_0"。
- 被索引字段的 .fnm attributes **必须**包含两项（实测 _0.fnm hexdump + PerFieldPostingsFormat 源码）：
  - `"PerFieldPostingsFormat.format"` = `"Lucene912"`
  - `"PerFieldPostingsFormat.suffix"` = `"0"`
  （reader 端 PerFieldPostingsFormat.FieldsReader 依据这两个属性定位格式与文件后缀；stored-only 字段不需要。）
- 实测确认：无 positions 时无 .pos/.pay；全 omitNorms 时无 .nvm/.nvd。

## 9. B3 实现精读要点（Lucene912PostingsWriter.java 逐行核对）

- **packed block 写盘顺序**（flushDocBlock:375-442）：先往 level0Output 写 skip 字段（VLong(impacts字节数)+impacts[+pos/pay skip]），此时记 `numSkipBytes=level0Output.size()`（**只含 skip 字段**）；再追加 ForDeltaUtil doc deltas + PForUtil freqs；然后 scratch = writeVInt15(docID-level0LastDocID) + writeVLong15(level0Output.size())（**此时的 size = skip 字段+doc块+freq块全长**，即 blockLength）；numSkipBytes += scratch.size()；最后 level1Output ← VLong(numSkipBytes) + scratch + level0Output。
- **写盘层次**：level1Output 攒满 32 块或 finishTerm 时落入 docOut；`(docCount & 4095) == 0` 时先 writeLevel1SkipData（VInt(docID-level1LastDocID) + [freqs: VLong(level1Len)+Short(scratch.size+2)+Short(numImpactBytes)+impacts(+pos/pay)] + level1Output）；finishTerm 收尾时无 level1 头直接 copy。
- **docCount 计数**：每 doc finishDoc 时 +1（跨 block 连续），level1 触发判断在 flushDocBlock 内。
- **writeVLong15**（:365-373）：`v & ~0x7FFF == 0` → writeShort(LE)；否则 writeShort(0x8000|(v&0x7FFF)) + writeVLong(v>>15)。
- **VInt tail**（PostingsUtil.writeVIntBlock:55-72）：写入 level0Output（无前缀），随后同 packed 块一样 copy 进 level1Output→docOut。每个值 `(docDelta<<1)|(freq==1?1:0)` GroupVInt；freq!=1 的追加 VInt freq。
- **singleton**（finishTerm:519-521）：docFreq==1 → `singletonDocID = docDeltaBuffer[0]-1`，.doc 无数据。
- **positions（M2 用）**：posDeltaBuffer 满 128 → pforUtil.encode 入 posOut；term 收尾 tail → 每值 `writeVInt(posDelta)`（无 payload/offset 时）。`lastPosBlockOffset = posOut.fp - posStartFP` 当 totalTermFreq > 128，否则 -1。skip 中 pos 字段 = `writeVLong(posOut.fp - level0LastPosFP) + writeByte(posBufferUpto)`。
- **encodeTerm**（:605-643）：absolute → lastState=EMPTY(docStartFP=0, singleton=-1)。特例：上一条与本条均 singleton 且 docStartFP 相同 → `writeVLong((zigzag(singletonDocID-prev)<<1)|1)`；否则 `writeVLong((docStartFP-prev)<<1)`，singleton 再 `writeVInt(singletonDocID)`。有 positions：`writeVLong(posStartFP-prev)`；lastPosBlockOffset != -1 再 `writeVLong(lastPosBlockOffset)`。
- **impacts 无 norms 情形**：所有 norm=1 → getCompetitiveFreqNormPairs 恒为单对 `(maxFreq, 1)` → writeImpacts 首对 prev=(0,0) → `writeVInt((maxFreq-1)<<1)`。psm 统计：maxNumImpacts*=1，maxImpactNumBytes* = 该 VInt 长度（1 或 2 字节）。实现时仍按通用 accumulator 写，便于将来支持 norms。
- **.psm**：4 个 LE int（maxNumImpactsAtLevel0, maxImpactNumBytesAtLevel0, maxNumImpactsAtLevel1, maxImpactNumBytesAtLevel1）+ LE long docOut 全长（**footer 写完后**的 fp，含 16B footer）+（有 .pos 再加 pos 全长）+ footer。
