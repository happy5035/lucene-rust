# FST Automaton Intersection Design

## 背景

当前 wildcard/prefix 查询（PrefixFilter/FullScan 分类）通过线性扫描 term 字典 + glob_match 过滤实现。对 enwiki 130K docs（字典 20 万+ term），FullScan 模式遍历全部 term，导致 wildcard 查询比 Java 慢 7-13x。

Java Lucene 用 `CompiledAutomaton` 与 FST/block-tree 交叉遍历（`IntersectTermsEnum`），在 FST 弧级剪枝整棵不匹配子树。本设计为 Rust 侧实现等价能力。

## 范围

- 仅支持 wildcard（`*` 任意多字符、`?` 一个字符）和 prefix
- 不需要通用正则/fuzzy（Levenshtein 自动机）
- PurePrefix（`foo*`）保持当前 seek_ceil + starts_with 路径（已足够快）
- PrefixFilter（`que?y3*`）和 FullScan（`*foo`）走 FST intersect

## 架构

```
┌─────────────────────────────────────────────────────────┐
│  multi_term.rs (core crate)                             │
│  collect_wildcard / collect_prefix                      │
│    → WildcardDfa::compile(pattern)                      │
│    → seg.intersect_terms(field, &dfa)                   │
└────────────────────────┬────────────────────────────────┘
                         │ LeafAccess::intersect_terms
┌────────────────────────▼────────────────────────────────┐
│  SegmentReader (disk)                                   │
│    → TermsDict::intersect(field_info, &dfa)             │
│      → FstReader::intersect(&dfa) → [(term, output)]   │
│      → scan_block(fp, term) → TermEntry                │
└─────────────────────────────────────────────────────────┘
┌─────────────────────────────────────────────────────────┐
│  MemoryLeafAccess (memory)                              │
│    → 默认实现：线性扫描 + DFA 过滤（字典小，够用）       │
└─────────────────────────────────────────────────────────┘
```

## 组件 1：WildcardDfa（自动机编译）

**文件**: `crates/codec-lucene9/src/automaton.rs`（新建）

```rust
pub struct WildcardDfa {
    /// transitions[state * 256 + byte] = next_state; DEAD = no transition
    transitions: Vec<u32>,
    accept: Vec<bool>,
    num_states: usize,
}

pub const DEAD: u32 = u32::MAX;

impl WildcardDfa {
    pub fn compile(pattern: &[u8]) -> Self;
    pub fn start(&self) -> u32;  // always 0
    pub fn is_accept(&self, state: u32) -> bool;
    pub fn transition(&self, state: u32, byte: u8) -> u32;
}
```

### 编译算法

逐字符扫描 pattern，维护当前状态集合（NFA 模拟展开为 DFA）：

- 普通字符 `c`：当前状态 --c--> 新状态（其余 byte → DEAD）
- `?`：当前状态 --任意 byte--> 新状态
- `*`：当前状态 --任意 byte--> 当前状态（自循环）；同时当前状态也是"下一个字符"的入口（ε 闭包）

实现方式：由于 wildcard 的 NFA 结构是线性链 + 自循环，可以直接构造 DFA 而不需要通用子集构造：

1. 第一遍：计算状态数（每个非 `*` 字符贡献一个状态，`*` 不新增状态但修改转移）
2. 第二遍：填充转移表
   - 遇到 `*`：当前状态对所有 256 byte 的转移指向自身，同时记录"ε 后继"（下一个非 `*` 字符的状态）
   - 遇到普通字符/`?`：从当前状态（含所有 ε 前驱）添加转移

状态数上界 = pattern.len() + 1。转移表大小 = states × 256 × 4 bytes。对 20 字符 pattern：21 × 1KB = 21KB。

### UTF-8 处理

`?` 匹配一个 Unicode code point。对多字节 UTF-8 字符（2-4 bytes），DFA 需要匹配完整的 byte 序列。处理方式：

- `?` 展开为：对 0x00-0x7F 单 byte 转移 + 对 0xC0-0xDF 开头走 2-byte 子自动机 + 0xE0-0xEF 走 3-byte + 0xF0-0xF7 走 4-byte
- 这会增加少量状态（每个 `?` 最多 +3 个辅助状态）
- 简化方案（推荐）：`?` 匹配任意单 byte。对 ASCII pattern + ASCII term（基准场景）完全正确；对多字节 term，`?` 匹配一个 byte 而非一个 code point。与 Java 有语义差异但实际影响极小（wildcard 查询几乎总是 ASCII）。

**决定**: 采用简化方案（`?` = 任意单 byte）。如果后续需要严格 code point 语义，再扩展。

## 组件 2：FST 交叉遍历（intersect_candidates）

**文件**: `crates/codec-lucene9/src/fst.rs`（扩展 FstReader）

FST 存储的是 term 前缀到 block fp 的映射（不是完整 term）。一个 final arc 表示"存在一个 block 包含此前缀开头的 term"。因此 intersect 分两阶段：FST 剪枝找候选 block → block 内 DFA 验证完整 term。

```rust
/// FST intersect 的候选结果：一个 block 及其对应的 DFA 状态。
pub struct IntersectCandidate {
    pub prefix: Vec<u8>,     // FST 累积的 term 前缀
    pub output: Vec<u8>,     // FST 累积输出（编码 block fp）
    pub depth: usize,        // 前缀长度（= FST 深度）
    pub dfa_state: u32,      // 到达此 final arc 时的 DFA 状态
}

impl FstReader {
    /// 沿 FST 下降，用 DFA 剪枝不匹配的 arc。
    /// 返回所有 final arc 处的候选 block（DFA 状态非 DEAD）。
    /// 调用方负责加载 block 并用 DFA 验证完整 term。
    pub fn intersect_candidates(
        &self,
        dfa: &WildcardDfa,
    ) -> io::Result<Vec<IntersectCandidate>>;
}
```

### 算法（迭代 DFS）

```
stack: Vec<(node_addr: u64, dfa_state: u32, prefix: Vec<u8>, output: Vec<u8>)>
candidates: Vec<IntersectCandidate>
scratch_arcs: Vec<FstArc>  // 复用（read_node_into）

push(start_node, 0, vec![], vec![])
while let Some((node, state, prefix, out)) = stack.pop():
    read_node_into(node, &mut scratch_arcs)
    for arc in &scratch_arcs:
        let next = dfa.transition(state, arc.label);
        if next == DEAD: continue;  // ← 剪枝：整棵子树跳过
        let mut new_prefix = prefix.clone();
        new_prefix.push(arc.label);
        let mut new_out = out.clone();
        if let Some(o) = &arc.output { new_out.extend_from_slice(o); }
        if arc.is_final:
            // 候选 block：DFA 走到这里还活着，block 内可能有匹配 term
            let mut full_out = new_out.clone();
            if let Some(fo) = &arc.final_output { full_out.extend_from_slice(fo); }
            candidates.push(IntersectCandidate {
                prefix: new_prefix.clone(),
                output: full_out,
                depth: new_prefix.len(),
                dfa_state: next,
            });
        if arc.target > 0:
            stack.push((arc.target as u64, next, new_prefix, new_out));
```

### 剪枝效果

- FST 节点的每个 arc 对应一个 label（byte）。DFA 对该 label 无转移 → 该 arc 下所有 term 跳过。
- 对 `b*t`：FST root 有 26 个 arc（a-z），DFA start 只对 'b' 有转移 → 25 棵子树跳过。
- 对 FullScan `*foo*`：start 对所有 byte 有转移（`*` 自循环），顶层不剪枝。但随着前缀积累，DFA 进入"需要匹配 foo"的状态后，不含 "foo" 的路径在后续层被剪。
- 关键：即使 final arc 处 DFA 不在 accept 状态，只要不是 DEAD，就要记录候选（block 内更长的 term 可能匹配）。

### 性能特征

- 访问的 FST 节点数 ∝ 匹配前缀数 × 平均深度（远小于字典总 term 数）
- 每次 `read_node_into` 复用 scratch Vec（已实现）
- prefix/output clone 深度 = FST 深度（通常 10-20），每次 clone 几十字节
- 候选数通常远小于字典 term 数（一个 block 含 25-48 个 term）

## 组件 3：TermsDict::intersect（block 加载 + term 验证）

**文件**: `crates/codec-lucene9/src/terms_read.rs`（扩展 TermsDict）

```rust
impl TermsDict {
    /// 枚举字段中被 DFA 接受的所有 term，返回 (term_bytes, TermEntry)。
    pub fn intersect(
        &mut self,
        field: &FieldInfo,
        dfa: &WildcardDfa,
    ) -> io::Result<Vec<(Vec<u8>, TermEntry)>>;
}
```

### 实现

1. `self.fst(field_index)?.intersect_candidates(dfa)` → `Vec<IntersectCandidate>`
2. 按 block fp 去重分组（多个候选可能指向同一 floor block）
3. 对每个唯一 block fp：
   - 解码 output 得到 fp（复用 `fst_output_block_fp` 逻辑：`code = MSB_VLong(output)`, `fp = code >> 2`）
   - 加载 block（复用 `load_frame_block` 的 block 解码逻辑）
   - 遍历 block 内所有 term entry：
     - 构建完整 term = candidate.prefix + entry.suffix
     - 从 candidate.dfa_state 开始，对 suffix 逐 byte 走 DFA 转移
     - DFA 在 suffix 结束时处于 accept 状态 → 匹配，收集 `(full_term, TermEntry)`
4. 返回所有匹配的 `(term, entry)`

### block 内 DFA 验证

```rust
fn dfa_accepts_suffix(dfa: &WildcardDfa, start_state: u32, suffix: &[u8]) -> bool {
    let mut state = start_state;
    for &b in suffix {
        state = dfa.transition(state, b);
        if state == DEAD { return false; }
    }
    dfa.is_accept(state)
}
```

### 去重优化

同一个 block 可能被多个 FST final arc 指向（floor block 结构）。按 fp 分组后，每个 block 只加载一次。对每个 block，可能有多个候选（不同 prefix + dfa_state），逐个验证。

实际中，对 PrefixFilter（有固定前缀），候选数通常 1-5 个。对 FullScan，候选数可能较多，但仍远小于字典总 block 数（DFA 在 FST 中下层剪枝）。

## 组件 4：LeafAccess 集成

**文件**: `crates/core/src/search/leaf_access.rs`

```rust
pub trait LeafAccess {
    // ... 现有方法 ...

    /// DFA 引导的 term 枚举。Disk 侧走 FST intersect；memory 侧走线性扫描。
    fn intersect_terms(
        &mut self,
        field: &str,
        dfa: &WildcardDfa,
    ) -> io::Result<Option<Vec<(Vec<u8>, Self::TermHandle)>>>;
}
```

- **SegmentReader**: 委托 `self.terms.intersect(fi, dfa)`，返回 `Vec<(Vec<u8>, TermEntry)>`
- **MemoryLeafAccess**: 线性扫描 sorted_ids，对每个 term 跑 `dfa.accepts(term_bytes)`，匹配的构建 MemTermHandle 返回

### multi_term.rs 调用方

```rust
WildcardClass::PrefixFilter | WildcardClass::FullScan => {
    let dfa = WildcardDfa::compile(&pat.pattern);
    let Some(results) = seg.intersect_terms(field, &dfa)? else {
        return Ok(None);
    };
    let mut collected = CollectedTerms { terms: vec![], entries: vec![] };
    for (term, handle) in results {
        let df = seg.term_doc_freq(&handle);
        collected.terms.push(term);
        collected.entries.push((df, handle));
    }
    collected.sort_by_df();
    Ok(Some((has_freqs, collected)))
}
```

## 测试策略

### 单元测试（codec-lucene9）

1. **DFA 编译**（`automaton.rs`）:
   - `compile(b"foo*")`: "foo"→accept, "foobar"→accept, "fo"→reject
   - `compile(b"a?e")`: "abe"→accept, "ae"→reject, "abbe"→reject
   - `compile(b"*foo")`: "foo"→accept, "barfoo"→accept, "foob"→reject
   - `compile(b"a*b*c")`: "abc"→accept, "aXbYc"→accept, "acb"→reject

2. **FST intersect**（`fst.rs`）:
   - 小 FST 上 intersect 结果 == 全量 lookup 过滤
   - 空结果 / 全匹配边界

3. **TermsDict intersect**（`terms_read.rs`）:
   - 多 term 索引上 intersect 结果 == 逐个 seek_exact 的结果集
   - TermEntry 一致性

### 集成测试（rustlucene-core）

4. **查询一致性**: wildcard query 走 intersect vs 旧线性扫描，hit count 逐位一致
5. **性能**: enwiki 基准 wildcard low/med/high 延迟降到 Java 2x 以内

## 文件清单

| 文件 | 变更 |
|------|------|
| `crates/codec-lucene9/src/automaton.rs` | 新建：WildcardDfa 编译 |
| `crates/codec-lucene9/src/fst.rs` | 扩展：FstReader::intersect |
| `crates/codec-lucene9/src/terms_read.rs` | 扩展：TermsDict::intersect |
| `crates/codec-lucene9/src/lib.rs` | 新增 `pub mod automaton;` |
| `crates/core/src/search/leaf_access.rs` | 新增 intersect_terms 方法 |
| `crates/core/src/search/segment_reader.rs` | 覆写 intersect_terms（disk） |
| `crates/core/src/memory_access.rs` | 覆写 intersect_terms（memory，线性） |
| `crates/core/src/search/multi_term.rs` | collect_wildcard 走 intersect |
