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
