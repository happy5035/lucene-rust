//! FST (finite state transducer) compiler, write side only, compatible with
//! Lucene 9.12.3 (`util/fst/FST.java`, `util/fst/FSTCompiler.java`,
//! `util/fst/ByteSequenceOutputs.java`).
//!
//! Scope mirrors how BlockTreeTermsWriter uses it:
//! - INPUT_TYPE.BYTE1 labels; outputs are byte strings (ByteSequenceOutputs:
//!   serialized as `VInt length + bytes`, empty bytes == NO_OUTPUT).
//! - No node dedup (`dedupHash == null`, FSTCompiler.java:180-183) and no
//!   fixed-length-arc nodes (`allowFixedLengthArcs == false`): every node is
//!   written in the unpacked, variable-length arc format, which the Java
//!   reader accepts because arc flags are self-describing.

use std::io;

use crate::codec_util::{self, check_header, corrupt, write_be_int};
use crate::io::{ChecksumIndexOutput, DataInput, IndexInput};

// Arc flag bits (FST.java:78-88).
const BIT_FINAL_ARC: u8 = 1 << 0;
const BIT_LAST_ARC: u8 = 1 << 1;
const BIT_TARGET_NEXT: u8 = 1 << 2;
const BIT_STOP_NODE: u8 = 1 << 3;
const BIT_ARC_HAS_OUTPUT: u8 = 1 << 4;
const BIT_ARC_HAS_FINAL_OUTPUT: u8 = 1 << 5;

// Virtual end-node addresses, never serialized (FST.java:130-136).
const FINAL_END_NODE: i64 = -1;
const NON_FINAL_END_NODE: i64 = 0;

// Metadata codec name and version (FST.java:111-125).
const FILE_FORMAT_NAME: &str = "FST";
const VERSION_CURRENT: u32 = 9; // VERSION_CONTINUOUS_ARCS == VERSION_CURRENT

/// DataOutput.writeVInt (DataOutput.java:198-204) into a scratch buffer.
fn write_vint_to_vec(out: &mut Vec<u8>, mut v: u32) {
    while v & !0x7f != 0 {
        out.push(((v & 0x7f) as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// DataOutput.writeVLong (DataOutput.java:236-250) into a scratch buffer.
fn write_vlong_to_vec(out: &mut Vec<u8>, mut v: u64) {
    while v & !0x7f != 0 {
        out.push(((v & 0x7f) as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// ByteSequenceOutputs.common (:43-72): longest common byte prefix;
/// an empty prefix is NO_OUTPUT.
fn outputs_common(a: &[u8], b: &[u8]) -> Option<Vec<u8>> {
    let n = a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count();
    if n == 0 { None } else { Some(a[..n].to_vec()) }
}

/// ByteSequenceOutputs.subtract (:74-93): strip the `inc` prefix from `output`.
fn outputs_subtract(output: Option<Vec<u8>>, inc: &Option<Vec<u8>>) -> Option<Vec<u8>> {
    match inc {
        None => output, // no prefix removed
        Some(inc) => {
            let output = output.expect("subtracted prefix must be a prefix of output");
            debug_assert!(output.starts_with(inc));
            let rest = &output[inc.len()..];
            if rest.is_empty() {
                None
            } else {
                Some(rest.to_vec())
            }
        }
    }
}

/// ByteSequenceOutputs.add (:95-112): concatenate prefix and output.
fn outputs_add(prefix: &Option<Vec<u8>>, output: Option<Vec<u8>>) -> Option<Vec<u8>> {
    match (prefix, output) {
        (None, o) => o,
        (Some(p), None) => Some(p.clone()),
        (Some(p), Some(o)) => {
            let mut r = p.clone();
            r.extend_from_slice(&o);
            Some(r)
        }
    }
}

/// A pending arc of an `UnCompiledNode` (FSTCompiler.Arc:1021-1028).
struct Arc {
    label: u8,
    target: Target,
    output: Option<Vec<u8>>,
    next_final_output: Option<Vec<u8>>,
    is_final: bool,
}

/// Arc target: the frontier node one level deeper (before it is frozen), or a
/// compiled node address (`<= 0` for the two virtual end nodes).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Target {
    Frontier,
    Compiled(i64),
}

/// A frontier node not yet serialized (FSTCompiler.UnCompiledNode:1059-1085).
#[derive(Default)]
struct UnCompiledNode {
    arcs: Vec<Arc>,
    output: Option<Vec<u8>>, // final output, valid when is_final
    is_final: bool,
}

impl UnCompiledNode {
    /// UnCompiledNode.addArc:1107-1128 (labels strictly increasing).
    fn add_arc(&mut self, label: u8) {
        debug_assert!(self.arcs.last().is_none_or(|a| label > a.label));
        self.arcs.push(Arc {
            label,
            target: Target::Frontier,
            output: None,
            next_final_output: None,
            is_final: false,
        });
    }

    /// UnCompiledNode.replaceLast:1130-1138.
    fn replace_last(
        &mut self,
        label: u8,
        target: i64,
        next_final_output: Option<Vec<u8>>,
        is_final: bool,
    ) {
        let arc = self
            .arcs
            .last_mut()
            .expect("replaceLast on node without arcs");
        debug_assert_eq!(arc.label, label);
        arc.target = Target::Compiled(target);
        arc.next_final_output = next_final_output;
        arc.is_final = is_final;
    }

    /// UnCompiledNode.prependOutput:1156-1168: push an output prefix onto all
    /// arcs (and onto this node's final output when it is an accepted prefix).
    fn prepend_output(&mut self, prefix: &Option<Vec<u8>>) {
        if prefix.is_none() {
            return; // add(NO_OUTPUT, x) == x
        }
        for arc in &mut self.arcs {
            arc.output = outputs_add(prefix, arc.output.take());
        }
        if self.is_final {
            self.output = outputs_add(prefix, self.output.take());
        }
    }
}

/// Incremental FST compiler (FSTCompiler with `dedupHash == null`).
///
/// Inputs must be added in globally ascending unsigned byte order. Each
/// frontier node is compiled exactly once, so `last_frozen_node` is always
/// the address of the previously serialized node (drives BIT_TARGET_NEXT).
pub struct FstCompiler {
    frontier: Vec<UnCompiledNode>,
    last_input: Vec<u8>,
    last_frozen_node: i64,
    // bytes[0] is the 0x00 padding so that no node gets address 0, which is
    // reserved for NON_FINAL_END_NODE (FSTCompiler.java:171-174, 573-577).
    // Java writes it lazily but pre-counts it; the final image is identical.
    bytes: Vec<u8>,
    empty_output: Option<Vec<u8>>,
}

impl Default for FstCompiler {
    fn default() -> Self {
        Self::new()
    }
}

impl FstCompiler {
    pub fn new() -> Self {
        FstCompiler {
            frontier: vec![UnCompiledNode::default()],
            last_input: Vec::new(),
            last_frozen_node: 0,
            bytes: vec![0],
            empty_output: None,
        }
    }

    /// Adds the next input/output pair (FSTCompiler.add:849-944).
    /// `None` (or an empty slice) means NO_OUTPUT.
    pub fn add(&mut self, input: &[u8], output: Option<&[u8]>) {
        let output: Option<Vec<u8>> = match output {
            Some(bytes) if !bytes.is_empty() => Some(bytes.to_vec()),
            _ => None, // empty BytesRef equals NO_OUTPUT (FSTCompiler.java:851-853)
        };

        if !self.last_input.is_empty() {
            // FSTCompiler.java:855-856; IntsRef.compareTo == unsigned byte order.
            assert!(
                input >= self.last_input.as_slice(),
                "inputs are added out of order"
            );
            assert!(
                input != self.last_input.as_slice(),
                "duplicate input: ByteSequenceOutputs does not implement merge"
            );
        }

        if input.is_empty() {
            // Empty input is only allowed as the first input (FSTCompiler.java:860-868).
            assert!(
                self.last_input.is_empty() && self.frontier[0].arcs.is_empty(),
                "empty input is only allowed as the first input"
            );
            assert!(self.empty_output.is_none(), "duplicate empty input");
            self.frontier[0].is_final = true;
            // setEmptyOutput:946-952 (merge unsupported, so first set wins only).
            self.empty_output = Some(output.unwrap_or_default());
            return;
        }

        // Shared prefix length with the previous input (FSTCompiler.java:871-879).
        let prefix_len = self
            .last_input
            .iter()
            .zip(input.iter())
            .take_while(|(a, b)| a == b)
            .count();
        let prefix_len_plus1 = prefix_len + 1;
        debug_assert!(prefix_len < input.len());

        while self.frontier.len() < input.len() + 1 {
            self.frontier.push(UnCompiledNode::default());
        }

        // Compile the orphan'd suffix of the previous input (FSTCompiler.java:891).
        self.freeze_tail(prefix_len_plus1);

        // Init tail states for the current input (FSTCompiler.java:894-896).
        for idx in prefix_len_plus1..=input.len() {
            self.frontier[idx - 1].add_arc(input[idx - 1]);
        }

        // FSTCompiler.java:898-902 (the duplicate branch is rejected up front).
        {
            let last_node = &mut self.frontier[input.len()];
            last_node.is_final = true;
            last_node.output = None;
        }

        // Push conflicting outputs forward, only as far as needed (:906-929).
        let mut output = output;
        for idx in 1..prefix_len_plus1 {
            let (parents, children) = self.frontier.split_at_mut(idx);
            let parent = &mut parents[idx - 1];
            let node = &mut children[0];
            let arc = parent.arcs.last_mut().expect("frontier arc missing");
            debug_assert_eq!(arc.label, input[idx - 1]); // getLastOutput:1101-1105
            if let Some(last_output) = arc.output.take() {
                let common = outputs_common(output.as_deref().unwrap_or(&[]), &last_output);
                let word_suffix = outputs_subtract(Some(last_output), &common);
                arc.output = common.clone(); // setLastOutput:1147-1153
                node.prepend_output(&word_suffix);
                output = outputs_subtract(output, &common);
            }
            // else: common prefix and word suffix are NO_OUTPUT, output unchanged.
        }

        // The new arc is private to this input, so the leftover output lands
        // on it (FSTCompiler.java:936-940).
        let arc = self.frontier[prefix_len_plus1 - 1]
            .arcs
            .last_mut()
            .expect("frontier arc missing");
        debug_assert_eq!(arc.label, input[prefix_len_plus1 - 1]);
        arc.output = output;

        self.last_input.clear();
        self.last_input.extend_from_slice(input);
    }

    /// Compiles the tail of the previous input that will not change anymore
    /// (FSTCompiler.freezeTail:813-839).
    fn freeze_tail(&mut self, prefix_len_plus1: usize) {
        let down_to = prefix_len_plus1.max(1);
        for idx in (down_to..=self.last_input.len()).rev() {
            let next_final_output = self.frontier[idx].output.clone();
            // A node with no outgoing arcs is made final on its incoming arc
            // (FSTCompiler.java:823-832).
            let is_final = self.frontier[idx].is_final || self.frontier[idx].arcs.is_empty();
            let node = std::mem::take(&mut self.frontier[idx]);
            let compiled = self.compile_node(&node);
            self.frontier[idx - 1].replace_last(
                self.last_input[idx - 1],
                compiled,
                next_final_output,
                is_final,
            );
        }
    }

    /// Serializes one node and returns its address, or `<= 0` for the virtual
    /// end nodes (FSTCompiler.compileNode:379-407 + addNode:411-567,
    /// variable-length-arcs path only).
    fn compile_node(&mut self, node: &UnCompiledNode) -> i64 {
        if node.arcs.is_empty() {
            return if node.is_final {
                FINAL_END_NODE
            } else {
                NON_FINAL_END_NODE
            };
        }

        // Scratch layout: arcs in ascending label order, each as
        // `[flags][label][output?][finalOutput?][target VLong?]` (:439-499).
        let mut scratch = Vec::new();
        let last_arc = node.arcs.len() - 1;
        for (arc_idx, arc) in node.arcs.iter().enumerate() {
            let target = match arc.target {
                Target::Compiled(t) => t,
                Target::Frontier => panic!("arc target must be compiled before its node"),
            };
            let mut flags = 0u8;
            if arc_idx == last_arc {
                flags |= BIT_LAST_ARC;
            }
            if self.last_frozen_node == target {
                // Only valid for non-fixed-length arcs (:450-455).
                flags |= BIT_TARGET_NEXT;
            }
            if arc.is_final {
                flags |= BIT_FINAL_ARC;
                if arc.next_final_output.is_some() {
                    flags |= BIT_ARC_HAS_FINAL_OUTPUT;
                }
            }
            let target_has_arcs = target > 0;
            if !target_has_arcs {
                flags |= BIT_STOP_NODE;
            }
            if arc.output.is_some() {
                flags |= BIT_ARC_HAS_OUTPUT;
            }

            scratch.push(flags);
            scratch.push(arc.label); // BYTE1 label (writeLabel:579-590)
            if let Some(output) = &arc.output {
                // ByteSequenceOutputs.write:115-119 (writeFinalOutput defaults
                // to write, Outputs.java:49-51).
                write_vint_to_vec(&mut scratch, output.len() as u32);
                scratch.extend_from_slice(output);
            }
            if let Some(final_output) = &arc.next_final_output {
                write_vint_to_vec(&mut scratch, final_output.len() as u32);
                scratch.extend_from_slice(final_output);
            }
            if target_has_arcs && flags & BIT_TARGET_NEXT == 0 {
                write_vlong_to_vec(&mut scratch, target as u64); // :495-499
            }
        }

        // The whole node is reversed into the output stream; its address is
        // the offset of its last byte (reverseScratchBytes:700-709, :557-566).
        scratch.reverse();
        self.bytes.extend_from_slice(&scratch);
        self.last_frozen_node = self.bytes.len() as i64 - 1;
        self.bytes.len() as i64 - 1
    }

    /// Freezes the last input's tail, compiles the root and returns the FST
    /// (FSTCompiler.compile:998-1019, finish:954-968).
    pub fn finish(mut self) -> Fst {
        self.freeze_tail(0);
        let root = std::mem::take(&mut self.frontier[0]);
        let root_addr = self.compile_node(&root);
        let start_node = if root_addr == FINAL_END_NODE && self.empty_output.is_some() {
            0 // FST accepts only the empty string (finish:959-961)
        } else {
            root_addr
        };
        debug_assert!(start_node >= 0);
        let num_bytes = self.bytes.len() as u64;
        Fst {
            bytes: self.bytes,
            start_node: start_node as u64,
            num_bytes,
            empty_output: self.empty_output,
        }
    }
}

/// A compiled FST image plus the metadata needed to read it.
pub struct Fst {
    bytes: Vec<u8>,
    start_node: u64,
    num_bytes: u64,
    empty_output: Option<Vec<u8>>,
}

impl Fst {
    /// Raw FST bytes, to be written into .tip at the field's indexStartFP.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Address of the root node (0 when the FST only accepts the empty string).
    pub fn start_node(&self) -> u64 {
        self.start_node
    }

    /// Total byte count, including the padding byte at offset 0.
    pub fn num_bytes(&self) -> u64 {
        self.num_bytes
    }

    /// Output of the empty string (`None` when the empty input is rejected).
    pub fn empty_output(&self) -> Option<&[u8]> {
        self.empty_output.as_deref()
    }

    /// Writes the FST metadata (FST.FSTMetadata.save:1220-1259):
    /// CodecUtil header ("FST", 9) + 1-byte emptyOutput flag (when set:
    /// VInt length + reversed serialized output) + 1-byte inputType
    /// (BYTE1 = 0) + VLong startNode + VLong numBytes.
    pub fn write_metadata(&self, out: &mut ChecksumIndexOutput) -> io::Result<()> {
        // CodecUtil.writeHeader (CodecUtil.java:77-86):
        // BE int magic + writeString(codec) + BE int version.
        write_be_int(out, codec_util::CODEC_MAGIC)?;
        out.write_string(FILE_FORMAT_NAME)?;
        write_be_int(out, VERSION_CURRENT)?;
        match &self.empty_output {
            Some(empty_output) => {
                out.write_byte(1)?;
                // Serialized final output, reversed wholesale (FST.java:1229-1244).
                let mut scratch = Vec::with_capacity(empty_output.len() + 5);
                write_vint_to_vec(&mut scratch, empty_output.len() as u32);
                scratch.extend_from_slice(empty_output);
                scratch.reverse();
                out.write_vint(scratch.len() as i32)?;
                out.write_bytes(&scratch)?;
            }
            None => out.write_byte(0)?,
        }
        out.write_byte(0)?; // INPUT_TYPE.BYTE1 (FST.java:1248-1256)
        out.write_vlong(self.start_node as i64)?;
        out.write_vlong(self.num_bytes as i64)?;
        Ok(())
    }
}

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
        arcs.iter()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::{ChecksumIndexOutput, IndexInput, IndexOutput};

    /// Minimal reverse reader for unpacked (variable-length) nodes, used to
    /// round-trip the compiler output. A node's address is the offset of its
    /// last byte; arcs are read backwards in ascending label order, mirroring
    /// FST.readArc (:943-991).
    struct ReadArc {
        label: u8,
        flags: u8,
        output: Option<Vec<u8>>,
        final_output: Option<Vec<u8>>,
        is_final: bool,
        is_last: bool,
        target: i64,
    }

    /// VInts/VLongs are written low-group-first, so reading the reversed bytes
    /// from the node's end reassembles them with the first byte read holding
    /// the lowest 7 bits.
    fn read_vlong_rev(bytes: &[u8], pos: &mut i64) -> u64 {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let b = bytes[*pos as usize];
            *pos -= 1;
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        v
    }

    /// ByteSequenceOutputs.read (:122-132) over reversed bytes.
    fn read_output_rev(bytes: &[u8], pos: &mut i64) -> Vec<u8> {
        let len = read_vlong_rev(bytes, pos) as usize;
        let end = *pos as usize;
        let start = end + 1 - len;
        let mut v = bytes[start..=end].to_vec();
        v.reverse();
        *pos = start as i64 - 1;
        v
    }

    /// Parses the node at `addr`; returns its arcs in label order and the
    /// position just below the node (= address of the node written
    /// immediately before it, which is what BIT_TARGET_NEXT resolves to).
    fn read_node(bytes: &[u8], addr: u64) -> (Vec<ReadArc>, i64) {
        let mut pos = addr as i64;
        let mut arcs = Vec::new();
        loop {
            let flags = bytes[pos as usize];
            pos -= 1;
            let label = bytes[pos as usize];
            pos -= 1;
            let output = if flags & BIT_ARC_HAS_OUTPUT != 0 {
                Some(read_output_rev(bytes, &mut pos))
            } else {
                None
            };
            let final_output = if flags & BIT_ARC_HAS_FINAL_OUTPUT != 0 {
                Some(read_output_rev(bytes, &mut pos))
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
                read_vlong_rev(bytes, &mut pos) as i64
            };
            let is_last = flags & BIT_LAST_ARC != 0;
            arcs.push(ReadArc {
                label,
                flags,
                output,
                final_output,
                is_final: flags & BIT_FINAL_ARC != 0,
                is_last,
                target,
            });
            if is_last {
                break;
            }
        }
        for arc in &mut arcs {
            if arc.target == i64::MIN {
                arc.target = pos;
            }
        }
        (arcs, pos)
    }

    /// Looks up an input, returning its full output (empty vec when the FST
    /// maps it to NO_OUTPUT) or `None` when the input is not accepted.
    fn lookup(fst: &Fst, input: &[u8]) -> Option<Vec<u8>> {
        if input.is_empty() {
            return fst.empty_output.clone();
        }
        let mut out = Vec::new();
        let mut node = fst.start_node as i64;
        for (i, &b) in input.iter().enumerate() {
            if node <= 0 {
                return None; // walked into an end node: input not accepted
            }
            let (arcs, _) = read_node(&fst.bytes, node as u64);
            let arc = arcs.iter().find(|a| a.label == b)?;
            if let Some(o) = &arc.output {
                out.extend_from_slice(o);
            }
            if i == input.len() - 1 {
                if !arc.is_final {
                    return None;
                }
                if let Some(fo) = &arc.final_output {
                    out.extend_from_slice(fo);
                }
                return Some(out);
            }
            if arc.target <= 0 {
                return None;
            }
            node = arc.target;
        }
        unreachable!()
    }

    /// Walks every node reachable from the root checking structural
    /// invariants of the serialized image.
    fn check_structure(fst: &Fst) {
        assert_eq!(fst.bytes[0], 0, "byte 0 is the padding byte");
        assert_eq!(fst.num_bytes, fst.bytes.len() as u64);
        if fst.start_node == 0 {
            return; // only the empty string is accepted; no nodes on disk
        }
        // The root is compiled last, so its address is the final byte offset.
        assert_eq!(fst.start_node, fst.bytes.len() as u64 - 1);
        let mut stack = vec![fst.start_node];
        let mut seen = std::collections::BTreeSet::new();
        while let Some(addr) = stack.pop() {
            if !seen.insert(addr) {
                continue;
            }
            let (arcs, prev_pos) = read_node(&fst.bytes, addr);
            assert!(
                arcs.last().unwrap().is_last,
                "last arc must have BIT_LAST_ARC"
            );
            assert!(arcs[..arcs.len() - 1].iter().all(|a| !a.is_last));
            for w in arcs.windows(2) {
                assert!(w[0].label < w[1].label, "arc labels ascending");
            }
            for arc in &arcs {
                if arc.flags & BIT_TARGET_NEXT != 0 {
                    // Only when the target is the previously frozen node
                    // (FSTCompiler.java:450-455).
                    assert!(arc.target > 0);
                    assert_eq!(arc.target, prev_pos);
                } else if arc.target > 0 {
                    assert_ne!(
                        arc.target, prev_pos,
                        "BIT_TARGET_NEXT required when target == last frozen node"
                    );
                }
                if arc.target > 0 {
                    stack.push(arc.target as u64);
                }
            }
        }
    }

    fn check_round_trip(entries: &[(&[u8], Option<&[u8]>)]) -> Fst {
        let mut compiler = FstCompiler::new();
        for (input, output) in entries {
            compiler.add(input, *output);
        }
        let fst = compiler.finish();
        check_structure(&fst);
        for (input, output) in entries {
            let expected = output.unwrap_or(&[]).to_vec();
            assert_eq!(
                lookup(&fst, input).as_deref(),
                Some(expected.as_slice()),
                "lookup {:?}",
                String::from_utf8_lossy(input)
            );
        }
        fst
    }

    #[test]
    fn round_trip_mixed() {
        let entries: &[(&[u8], Option<&[u8]>)] = &[
            (b"a", Some(b"\x01")),
            (b"ab", Some(b"\x02\x03")),
            (b"abc", Some(b"\x02")),
            (b"b", Some(b"\x05")),
            (b"ca", None),
            (b"cabd", Some(b"\x09\x09\x09")),
        ];
        let fst = check_round_trip(entries);
        assert_eq!(lookup(&fst, b""), None);
        assert_eq!(lookup(&fst, b"ac"), None);
        assert_eq!(lookup(&fst, b"abb"), None);
        assert_eq!(lookup(&fst, b"bX"), None);
        assert_eq!(lookup(&fst, b"d"), None);
        assert_eq!(fst.empty_output(), None);
    }

    #[test]
    fn final_arc_mid_node() {
        // A prefix that is itself accepted ("ab") alongside longer inputs:
        // the 'b' arc must carry BIT_FINAL_ARC plus the final output.
        let entries: &[(&[u8], Option<&[u8]>)] = &[
            (b"ab", Some(b"W")),
            (b"abc", Some(b"XYZ")),
            (b"abd", Some(b"XY")),
        ];
        check_round_trip(entries);
    }

    #[test]
    fn fsa_mode_no_outputs() {
        let entries: &[(&[u8], Option<&[u8]>)] = &[
            (b"cat", None),
            (b"cats", None),
            (b"dog", None),
            (b"dogs", None),
        ];
        let fst = check_round_trip(entries);
        assert_eq!(lookup(&fst, b"cat"), Some(vec![]));
        assert_eq!(lookup(&fst, b"ca"), None);
        assert_eq!(lookup(&fst, b"do"), None);
    }

    #[test]
    fn single_input() {
        let fst = check_round_trip(&[(b"hello", Some(b"world"))]);
        assert_eq!(lookup(&fst, b"hell"), None);
        assert_eq!(lookup(&fst, b"helloo"), None);
    }

    #[test]
    fn empty_string_first() {
        let entries: &[(&[u8], Option<&[u8]>)] =
            &[(b"", Some(b"rc")), (b"a", Some(b"1")), (b"ab", Some(b"2"))];
        let fst = check_round_trip(entries);
        assert_eq!(fst.empty_output(), Some(&b"rc"[..]));
        assert_eq!(lookup(&fst, b""), Some(b"rc".to_vec()));
    }

    #[test]
    fn only_empty_input_no_output() {
        let mut compiler = FstCompiler::new();
        compiler.add(b"", None);
        let fst = compiler.finish();
        assert_eq!(fst.start_node(), 0);
        assert_eq!(fst.bytes(), &[0u8]);
        assert_eq!(fst.num_bytes(), 1);
        assert_eq!(fst.empty_output(), Some(&[][..]));
        assert_eq!(lookup(&fst, b""), Some(vec![]));
        assert_eq!(lookup(&fst, b"a"), None);
    }

    #[test]
    fn exact_byte_layout() {
        // "ab" -> X, "ac" -> Y, hand-computed image (see FSTCompiler.addNode).
        let fst = check_round_trip(&[(b"ab", Some(b"X")), (b"ac", Some(b"Y"))]);
        let expected: &[u8] = &[
            0x00, // padding
            // node for arcs 'b'/'c' (reversed scratch):
            0x59, 0x01, 0x63, 0x1B, // 'c': FINAL|LAST|STOP|HAS_OUTPUT, output len 1 "Y"
            0x58, 0x01, 0x62, 0x19, // 'b': FINAL|STOP|HAS_OUTPUT, output len 1 "X"
            // root: arc 'a', LAST|TARGET_NEXT, no target bytes:
            0x61, 0x06,
        ];
        assert_eq!(fst.bytes(), expected);
        assert_eq!(fst.start_node(), 10);
        assert_eq!(fst.num_bytes(), 11);
    }

    #[test]
    fn target_next_flag_placement() {
        let fst = check_round_trip(&[(b"ab", Some(b"X")), (b"ac", Some(b"Y"))]);
        let (root_arcs, _) = read_node(fst.bytes(), fst.start_node());
        assert_eq!(root_arcs.len(), 1);
        // The root's only arc targets the node compiled just before it.
        assert_ne!(root_arcs[0].flags & BIT_TARGET_NEXT, 0);
        assert_eq!(root_arcs[0].flags & BIT_STOP_NODE, 0);
        let (child_arcs, _) = read_node(fst.bytes(), root_arcs[0].target as u64);
        assert_eq!(child_arcs.len(), 2);
        for arc in &child_arcs {
            assert_eq!(arc.flags & BIT_TARGET_NEXT, 0);
            assert_ne!(arc.flags & BIT_STOP_NODE, 0); // both target the final end node
        }
    }

    #[test]
    fn generated_round_trip() {
        // Deterministic xorshift64* PRNG.
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

        let mut compiler = FstCompiler::new();
        for (input, output) in &entries {
            compiler.add(input, output.as_deref());
        }
        let fst = compiler.finish();
        check_structure(&fst);
        assert!(fst.num_bytes() > 127, "exercises multi-byte VLong targets");
        for (input, output) in &entries {
            let expected = output.clone().unwrap_or_default();
            assert_eq!(lookup(&fst, input).as_deref(), Some(expected.as_slice()));
        }
        // Misses: label outside the used alphabet, and an over-long input.
        assert_eq!(lookup(&fst, b"z"), None);
        assert_eq!(lookup(&fst, &[b'a'; 13]), None);
    }

    #[test]
    fn metadata_layout_with_empty_output() {
        let mut compiler = FstCompiler::new();
        compiler.add(b"", Some(b"xy"));
        let fst = compiler.finish();
        assert_eq!(fst.start_node(), 0);
        assert_eq!(fst.bytes(), &[0u8]);

        let mut out = ChecksumIndexOutput::new(IndexOutput::in_memory());
        fst.write_metadata(&mut out).unwrap();
        let bytes = out.into_bytes();

        let mut expected = vec![0x3f, 0xd7, 0x6c, 0x17]; // BE CODEC_MAGIC
        expected.push(3); // "FST" string length
        expected.extend_from_slice(b"FST");
        expected.extend_from_slice(&[0, 0, 0, 9]); // BE version 9
        expected.push(1); // emptyOutput present
        // Serialized output is [VInt 2, 'x', 'y'], reversed wholesale:
        expected.push(3);
        expected.extend_from_slice(&[b'y', b'x', 0x02]);
        expected.push(0); // INPUT_TYPE.BYTE1
        expected.push(0); // VLong startNode = 0
        expected.push(1); // VLong numBytes = 1
        assert_eq!(bytes, expected);
    }

    #[test]
    fn metadata_layout_without_empty_output() {
        let fst = check_round_trip(&[(b"ab", Some(b"X")), (b"ac", Some(b"Y"))]);
        let mut out = ChecksumIndexOutput::new(IndexOutput::in_memory());
        fst.write_metadata(&mut out).unwrap();
        let bytes = out.into_bytes();

        let mut expected = vec![0x3f, 0xd7, 0x6c, 0x17];
        expected.push(3);
        expected.extend_from_slice(b"FST");
        expected.extend_from_slice(&[0, 0, 0, 9]);
        expected.push(0); // no emptyOutput
        expected.push(0); // INPUT_TYPE.BYTE1
        expected.push(10); // VLong startNode
        expected.push(11); // VLong numBytes
        assert_eq!(bytes, expected);
    }

    #[test]
    #[should_panic(expected = "out of order")]
    fn panics_on_unsorted_input() {
        let mut compiler = FstCompiler::new();
        compiler.add(b"b", None);
        compiler.add(b"a", None);
    }

    #[test]
    #[should_panic(expected = "duplicate input")]
    fn panics_on_duplicate_input() {
        let mut compiler = FstCompiler::new();
        compiler.add(b"a", Some(b"x"));
        compiler.add(b"a", Some(b"y"));
    }

    #[test]
    #[should_panic(expected = "out of order")]
    fn panics_on_late_empty_input() {
        let mut compiler = FstCompiler::new();
        compiler.add(b"a", None);
        compiler.add(b"", None);
    }

    #[test]
    #[should_panic(expected = "duplicate empty input")]
    fn panics_on_duplicate_empty_input() {
        let mut compiler = FstCompiler::new();
        compiler.add(b"", None);
        compiler.add(b"", Some(b"x"));
    }

    fn fst_reader(entries: &[(&[u8], Option<&[u8]>)]) -> (FstReader, Vec<u8>) {
        let mut compiler = FstCompiler::new();
        for (input, output) in entries {
            compiler.add(input, *output);
        }
        let fst = compiler.finish();
        let mut out = ChecksumIndexOutput::new(IndexOutput::in_memory());
        fst.write_metadata(&mut out).unwrap();
        let meta_bytes = out.into_bytes();
        let metadata = FstMetadata::read(&mut IndexInput::in_memory(meta_bytes)).unwrap();
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
                Some(output.unwrap_or(&[])),
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
        // block-tree usage: output = block pointer encoding; final arcs on
        // the path give candidate frames
        let entries: &[(&[u8], Option<&[u8]>)] = &[
            (b"", Some(b"R")),
            (b"ab", Some(b"X")),
            (b"abc", Some(b"Y")),
            (b"b", Some(b"Z")),
        ];
        let (reader, _) = fst_reader(entries);
        // "abc" full path: depth 2 ("ab") and depth 3 ("abc") both final
        assert_eq!(
            reader.trace_path(b"abc").unwrap(),
            vec![(2usize, b"X".to_vec()), (3usize, b"Y".to_vec())]
        );
        // "abd" walks to depth 2 then no arc: only ("ab", X)
        assert_eq!(
            reader.trace_path(b"abd").unwrap(),
            vec![(2usize, b"X".to_vec())]
        );
        // "c" no arc at root byte: empty
        assert_eq!(
            reader.trace_path(b"c").unwrap(),
            Vec::<(usize, Vec<u8>)>::new()
        );
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
}
