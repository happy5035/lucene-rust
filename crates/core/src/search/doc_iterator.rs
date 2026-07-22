use codec_lucene9::postings_reader::{PostingsEnum, NO_MORE_DOCS};

/// A block of decoded doc IDs from a posting list.
/// 128 values matching PFOR block size; stack-allocated.
pub struct DocBlock {
    pub docs: [u32; 128],
    pub len: u8, // 1..=128, 0 = exhausted
}

impl DocBlock {
    pub fn empty() -> Self {
        DocBlock {
            docs: [0u32; 128],
            len: 0,
        }
    }
}

pub trait DocIterator: Send {
    fn next(&mut self) -> Option<u32>;
    fn advance(&mut self, target: u32) -> Option<u32>;
    fn cost(&self) -> usize;
    fn next_block(&mut self) -> Option<DocBlock> {
        let mut block = DocBlock::empty();
        for i in 0..128 {
            match self.next() {
                Some(d) => {
                    block.docs[i] = d;
                    block.len += 1;
                }
                None => break,
            }
        }
        if block.len == 0 {
            None
        } else {
            Some(block)
        }
    }
}

/// Wraps a PostingsEnum to yield doc IDs via DocIterator.
pub struct PostingsDocIterator {
    pe: PostingsEnum,
    exhausted: bool,
}

impl PostingsDocIterator {
    pub fn new(pe: PostingsEnum) -> Self {
        PostingsDocIterator {
            pe,
            exhausted: false,
        }
    }
}

impl DocIterator for PostingsDocIterator {
    fn next(&mut self) -> Option<u32> {
        if self.exhausted {
            return None;
        }
        match self.pe.next_doc() {
            Ok(doc) if doc != NO_MORE_DOCS => Some(doc as u32),
            _ => {
                self.exhausted = true;
                None
            }
        }
    }

    fn advance(&mut self, target: u32) -> Option<u32> {
        if self.exhausted {
            return None;
        }
        match self.pe.advance(target as i32) {
            Ok(doc) if doc != NO_MORE_DOCS => Some(doc as u32),
            _ => {
                self.exhausted = true;
                None
            }
        }
    }

    fn cost(&self) -> usize {
        1 // placeholder
    }
}
