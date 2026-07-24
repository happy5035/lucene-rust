import java.nio.file.*;
import org.apache.lucene.analysis.core.WhitespaceAnalyzer;
import org.apache.lucene.index.*;
import org.apache.lucene.store.*;

/**
 * M3 battery tool: forceMerge(1) an existing index in place (used on a
 * Rust-written --bitmap index). The merge re-encodes postings via
 * PostingsEnum, so the merged segment carries no inline bitmaps (spec
 * §4a.4) and the Rust read side naturally falls back to postings (tier 3);
 * the search battery re-run afterwards must produce identical results.
 * Non-compound output (matches JavaLogBench's setUseCompoundFile(false),
 * JavaLogBench.java:91) — the Rust reader has no CFS support.
 *
 * Usage: ForceMergeIndex <indexDir>
 */
public class ForceMergeIndex {
    public static void main(String[] args) throws Exception {
        Path indexDir = Paths.get(args[0]);
        try (Directory dir = FSDirectory.open(indexDir)) {
            IndexWriterConfig cfg = new IndexWriterConfig(new WhitespaceAnalyzer())
                .setOpenMode(IndexWriterConfig.OpenMode.APPEND)
                .setUseCompoundFile(false);
            try (IndexWriter w = new IndexWriter(dir, cfg)) {
                w.forceMerge(1);
                w.commit();
            }
        }
        System.out.println("FORCEMERGE_OK");
    }
}
