import java.nio.file.*;
import java.util.*;
import org.apache.lucene.document.*;
import org.apache.lucene.index.*;
import org.apache.lucene.search.*;
import org.apache.lucene.store.*;
import org.apache.lucene.util.BytesRef;

/**
 * Canonical query dump for the M2 log schema. Runs a fixed, deterministic
 * query battery against an index and prints machine-comparable results.
 * Diffing the output of two runs (Rust-written vs Java-written index over the
 * same corpus) verifies format compatibility end to end.
 *
 * All queries are wrapped in ConstantScoreQuery where scoring could apply —
 * the index carries no norms, so scores are meaningless by design.
 *
 * Usage: VerifyLogIndex <indexDir> [expectPositions]
 */
public class VerifyLogIndex {
    public static void main(String[] args) throws Exception {
        Path indexDir = Paths.get(args[0]);
        boolean expectPositions = args.length > 1 && Boolean.parseBoolean(args[1]);
        StringBuilder out = new StringBuilder();

        try (Directory dir = FSDirectory.open(indexDir);
             IndexReader r = DirectoryReader.open(dir)) {
            IndexSearcher s = new IndexSearcher(r);
            out.append("maxDoc=").append(r.maxDoc()).append('\n');

            // --- keyword term queries (postings, IndexOptions.DOCS) ---
            for (String level : new String[]{"INFO","WARN","ERROR","DEBUG","TRACE"}) {
                Query q = new ConstantScoreQuery(new TermQuery(new Term("level", level)));
                out.append("term level=").append(level)
                   .append(" count=").append(s.count(q)).append('\n');
            }

            // --- trace_id point lookup (exactly one hit expected) ---
            StoredFields stored = s.storedFields();
            if (r.maxDoc() > 7) {
                String tid = stored.document(7).get("trace_id");
                Query q = new ConstantScoreQuery(new TermQuery(new Term("trace_id", tid)));
                out.append("term trace_id(doc7) count=").append(s.count(q)).append('\n');
            }

            // --- points: LongPoint range between the stored ts of two docs ---
            if (r.maxDoc() > 200) {
                long lo = stored.document(100).getField("timestamp").numericValue().longValue();
                long hi = stored.document(199).getField("timestamp").numericValue().longValue();
                Query q = new ConstantScoreQuery(LongPoint.newRangeQuery("timestamp", lo, hi));
                int count = s.count(q);
                out.append("range timestamp[").append(lo).append(',').append(hi)
                   .append("] count=").append(count).append('\n');
                TopDocs td = s.search(q, 20, Sort.INDEXORDER);
                StringBuilder ids = new StringBuilder();
                for (ScoreDoc sd : td.scoreDocs) ids.append(sd.doc).append(',');
                out.append("range first20=").append(ids).append('\n');
                // full-range extremes: min/max via sorted search
                Query all = new ConstantScoreQuery(LongPoint.newRangeQuery("timestamp", Long.MIN_VALUE, Long.MAX_VALUE));
                out.append("range all count=").append(s.count(all)).append('\n');
            }

            // --- sort by NumericDocValues timestamp (top 10, ascending) ---
            {
                Sort sort = new Sort(new SortField("timestamp", SortField.Type.LONG));
                TopDocs td = s.search(new ConstantScoreQuery(new MatchAllDocsQuery()), 10, sort);
                StringBuilder b = new StringBuilder();
                for (ScoreDoc sd : td.scoreDocs) {
                    long ts = stored.document(sd.doc).getField("timestamp").numericValue().longValue();
                    b.append(sd.doc).append(':').append(ts).append(',');
                }
                out.append("sort_ts_asc_top10=").append(b).append('\n');
            }

            // --- SortedDocValues dictionary (level): ords must be sorted ---
            for (LeafReaderContext ctx : r.leaves()) {
                SortedDocValues sdv = ctx.reader().getSortedDocValues("level");
                if (sdv == null) continue;
                out.append("level_dict size=").append(sdv.getValueCount()).append(' ');
                for (int ord = 0; ord < sdv.getValueCount(); ord++) {
                    BytesRef t = sdv.lookupOrd(ord);
                    out.append(t.utf8ToString()).append(',');
                }
                out.append('\n');
            }

            // --- high-cardinality SortedDocValues (trace_id_sdv, --bigdict) ---
            for (LeafReaderContext ctx : r.leaves()) {
                SortedDocValues sdv = ctx.reader().getSortedDocValues("trace_id_sdv");
                if (sdv == null) continue;
                int n = sdv.getValueCount();
                out.append("trace_sdv_dict size=").append(n).append(" bounds=")
                   .append(sdv.lookupOrd(0).utf8ToString()).append("..")
                   .append(sdv.lookupOrd(n - 1).utf8ToString()).append('\n');
                // hash the whole dictionary for a compact but complete diff
                long h = 1125899906842597L; // prime
                for (int ord = 0; ord < n; ord++) {
                    BytesRef t = sdv.lookupOrd(ord);
                    for (int i = t.offset; i < t.offset + t.length; i++) {
                        h = 31 * h + (t.bytes[i] & 0xff);
                    }
                }
                out.append("trace_sdv_dict hash=").append(h).append('\n');
            }

            // --- NumericDocValues spot check + per-field cardinality ---
            for (String f : new String[]{"timestamp","latency_ms","bytes_sent","status"}) {
                long card = 0;
                for (LeafReaderContext ctx : r.leaves()) {
                    NumericDocValues ndv = ctx.reader().getNumericDocValues(f);
                    if (ndv != null) card += ndv.cost();
                }
                out.append("dv_card ").append(f).append('=').append(card).append('\n');
            }
            for (LeafReaderContext ctx : r.leaves()) {
                NumericDocValues ndv = ctx.reader().getNumericDocValues("latency_ms");
                if (ndv == null) continue;
                StringBuilder b = new StringBuilder();
                for (int d = 0; d + ctx.docBase < Math.min(9, r.maxDoc()); d++) {
                    if (ndv.advanceExact(d)) {
                        b.append(ctx.docBase + d).append(':').append(ndv.longValue()).append(',');
                    } else {
                        b.append(ctx.docBase + d).append(":MISSING,");
                    }
                }
                out.append("latency first9=").append(b).append('\n');
                break;
            }

            // --- message term + phrase (positions only for phrase) ---
            {
                String msg = stored.document(0).get("message");
                String[] toks = msg.trim().split("\\s+");
                Query q = new ConstantScoreQuery(new TermQuery(new Term("message", toks[0])));
                out.append("term message=").append(toks[0])
                   .append(" count=").append(s.count(q)).append('\n');
                if (expectPositions && toks.length >= 2) {
                    PhraseQuery pq = new PhraseQuery("message", toks[0], toks[1]);
                    out.append("phrase(\"").append(toks[0]).append(' ').append(toks[1])
                       .append("\") count=").append(s.count(pq)).append('\n');
                }
            }
        }
        System.out.print(out);
    }
}
