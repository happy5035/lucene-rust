import java.nio.file.*;
import org.apache.lucene.document.*;
import org.apache.lucene.index.*;
import org.apache.lucene.search.*;
import org.apache.lucene.store.*;
import org.apache.lucene.util.BytesRef;

/**
 * Search battery for the M1 Rust read path: fixed Term/MatchAll queries with
 * results printed in the exact format of `rustlucene-cli searchdump`.
 * Diffing the two outputs validates the Rust reader against Java Lucene
 * 9.12.3 (counts, docID sequences, freq sums, singleton df=1 path).
 *
 * Usage: VerifySearchIndex <indexDir>
 */
public class VerifySearchIndex {
    public static void main(String[] args) throws Exception {
        Path indexDir = Paths.get(args[0]);
        boolean positions = args.length > 1 && args[1].equals("--positions");
        StringBuilder out = new StringBuilder();

        try (Directory dir = FSDirectory.open(indexDir);
             IndexReader r = DirectoryReader.open(dir)) {
            IndexSearcher s = new IndexSearcher(r);
            out.append("maxDoc=").append(r.maxDoc()).append('\n');

            // keyword terms (IndexOptions.DOCS), df ~40k crosses the 4096-doc
            // level-1 skip boundary
            for (String level : new String[]{"INFO","WARN","ERROR","DEBUG","TRACE"}) {
                Query q = new ConstantScoreQuery(new TermQuery(new Term("level", level)));
                out.append("term level=").append(level)
                   .append(" count=").append(s.count(q)).append('\n');
            }
            {
                Query q = new ConstantScoreQuery(new TermQuery(new Term("level", "INFO")));
                TopDocs td = s.search(q, 20, Sort.INDEXORDER);
                StringBuilder b = new StringBuilder();
                for (ScoreDoc sd : td.scoreDocs) b.append(sd.doc).append(',');
                out.append("term level=INFO first20=").append(b).append('\n');
            }

            // text terms (DOCS_AND_FREQS): count + full freq sum
            for (String w : new String[]{"connection0","query23","queue39"}) {
                Query q = new ConstantScoreQuery(new TermQuery(new Term("message", w)));
                long freqsum = 0;
                PostingsEnum pe = MultiTerms.getTermPostingsEnum(
                    r, "message", new BytesRef(w), PostingsEnum.FREQS);
                if (pe != null) {
                    while (pe.nextDoc() != PostingsEnum.NO_MORE_DOCS) freqsum += pe.freq();
                }
                out.append("term message=").append(w)
                   .append(" count=").append(s.count(q))
                   .append(" freqsum=").append(freqsum).append('\n');
            }
            {
                Query q = new ConstantScoreQuery(new TermQuery(new Term("message", "nosuchterm42")));
                out.append("term message=nosuchterm42 count=").append(s.count(q)).append('\n');
            }

            // df=1 singleton: doc7's trace_id from stored fields (ground truth)
            StoredFields stored = s.storedFields();
            if (r.maxDoc() > 7) {
                String tid = stored.document(7).get("trace_id");
                Query q = new ConstantScoreQuery(new TermQuery(new Term("trace_id", tid)));
                out.append("term trace_id(doc7)=").append(tid)
                   .append(" count=").append(s.count(q)).append('\n');
            }

            {
                Query q = new MatchAllDocsQuery();
                out.append("matchall count=").append(s.count(q)).append('\n');
                TopDocs td = s.search(q, 20, Sort.INDEXORDER);
                StringBuilder b = new StringBuilder();
                for (ScoreDoc sd : td.scoreDocs) b.append(sd.doc).append(',');
                out.append("matchall first20=").append(b).append('\n');
            }

            // Boolean battery (search spec phase 3): "and" = MUST+MUST,
            // "or" = SHOULD+SHOULD. Same items/format as the boolean_battery
            // in rustlucene-cli searchdump.
            String[][][] battery = {
                {{"and"}, {"message"}, {"connection0", "query23"}},
                {{"and"}, {"level"}, {"INFO", "WARN"}},
                {{"or"}, {"message"}, {"connection0", "query23"}},
                {{"or"}, {"message"}, {"connection0", "nosuchterm42"}},
                {{"and"}, {"message"}, {"connection0", "nosuchterm42"}},
            };
            for (String[][] item : battery) {
                String op = item[0][0], field = item[1][0];
                BooleanClause.Occur occur = op.equals("and")
                    ? BooleanClause.Occur.MUST : BooleanClause.Occur.SHOULD;
                BooleanQuery.Builder bq = new BooleanQuery.Builder();
                for (String t : item[2])
                    bq.add(new TermQuery(new Term(field, t)), occur);
                Query q = new ConstantScoreQuery(bq.build());
                TopDocs td = s.search(q, 20, Sort.INDEXORDER);
                StringBuilder b = new StringBuilder();
                for (ScoreDoc sd : td.scoreDocs) b.append(sd.doc).append(',');
                out.append(op).append(' ').append(field).append('=')
                   .append(String.join(",", item[2]))
                   .append(" count=").append(s.count(q))
                   .append(" first20=").append(b).append('\n');
            }

            // M2 Terms(IN) battery: same items/format as the terms_battery in
            // rustlucene-cli searchdump. Boolean SHOULD of TermQuery is the
            // Java counterpart of Rust's Terms dual-path execution.
            String[][][] termsBattery = {
                {{"level"}, {"INFO", "WARN", "DEBUG"}},
                {{"message"}, {"connection0", "query23", "queue39"}},
                {{"message"}, {"connection0", "nosuchterm42"}},
                {{"message"}, {"connection0","connection1","connection2","connection3",
                                "connection4","connection5","connection6","connection7",
                                "connection8","connection9","connection10","connection11",
                                "connection12","connection13","connection14","connection15",
                                "connection16"}},
            };
            for (String[][] item : termsBattery) {
                String field = item[0][0];
                BooleanQuery.Builder bq = new BooleanQuery.Builder();
                for (String t : item[1])
                    bq.add(new TermQuery(new Term(field, t)), BooleanClause.Occur.SHOULD);
                Query q = new ConstantScoreQuery(bq.build());
                TopDocs td = s.search(q, 20, Sort.INDEXORDER);
                StringBuilder b = new StringBuilder();
                for (ScoreDoc sd : td.scoreDocs) b.append(sd.doc).append(',');
                out.append("terms ").append(field).append('=')
                   .append(String.join(",", item[1]))
                   .append(" count=").append(s.count(q))
                   .append(" first20=").append(b).append('\n');
            }

            // M2 prefix battery: same items/format as searchdump. PrefixQuery
            // is Lucene's counterpart of the Rust TermsIter-driven expansion.
            String[][] prefixBattery = {
                {"level", "IN"},
                {"message", "connection3"},
                {"message", "conn"},
                {"message", "zzzz"},
            };
            for (String[] item : prefixBattery) {
                Query q = new ConstantScoreQuery(new PrefixQuery(new Term(item[0], item[1])));
                TopDocs td = s.search(q, 20, Sort.INDEXORDER);
                StringBuilder b = new StringBuilder();
                for (ScoreDoc sd : td.scoreDocs) b.append(sd.doc).append(',');
                out.append("prefix ").append(item[0]).append('=').append(item[1])
                   .append(" count=").append(s.count(q))
                   .append(" first20=").append(b).append('\n');
            }
            if (r.maxDoc() > 7) {
                String tid8 = stored.document(7).get("trace_id").substring(0, 8);
                Query q = new ConstantScoreQuery(new PrefixQuery(new Term("trace_id", tid8)));
                out.append("prefix trace_id=").append(tid8)
                   .append(" count=").append(s.count(q)).append('\n');
            }

            // M2 wildcard battery: same items/format as searchdump.
            String[][] wildcardBattery = {
                {"message", "connection*"},
                {"message", "que?y3*"},
                {"message", "*onnection1"},
                {"message", "*zzz"},
            };
            for (String[] item : wildcardBattery) {
                Query q = new ConstantScoreQuery(new WildcardQuery(new Term(item[0], item[1])));
                TopDocs td = s.search(q, 20, Sort.INDEXORDER);
                StringBuilder b = new StringBuilder();
                for (ScoreDoc sd : td.scoreDocs) b.append(sd.doc).append(',');
                out.append("wildcard ").append(item[0]).append('=').append(item[1])
                   .append(" count=").append(s.count(q))
                   .append(" first20=").append(b).append('\n');
            }
            // M2 phrase battery (positions variant only): same items/format
            // as searchdump. The phrase terms are doc7's real adjacent tokens
            // (a guaranteed hit), read from stored fields.
            if (positions && r.maxDoc() > 7) {
                String[] toks = stored.document(7).get("message").split(" ");
                String t0 = toks[0], t1 = toks[1], t2 = toks[2];
                {
                    Query q = new ConstantScoreQuery(new PhraseQuery("message", t0, t1));
                    TopDocs td = s.search(q, 20, Sort.INDEXORDER);
                    StringBuilder b = new StringBuilder();
                    for (ScoreDoc sd : td.scoreDocs) b.append(sd.doc).append(',');
                    out.append("phrase message=").append(t0).append(',').append(t1)
                       .append(" count=").append(s.count(q))
                       .append(" first20=").append(b).append('\n');
                }
                {
                    Query q = new ConstantScoreQuery(new PhraseQuery("message", t0, t1, t2));
                    out.append("phrase message=").append(t0).append(',').append(t1).append(',').append(t2)
                       .append(" count=").append(s.count(q)).append('\n');
                }
                {
                    Query q = new ConstantScoreQuery(new PhraseQuery("message", t1, t0));
                    out.append("phrase message=").append(t1).append(',').append(t0)
                       .append(" count=").append(s.count(q)).append('\n');
                }
                String[][] degenerate = {
                    {"query23", "query23"},
                    {"connection0", "nosuchterm42"},
                    {"connection0"},
                };
                for (String[] terms : degenerate) {
                    Query q = new ConstantScoreQuery(new PhraseQuery("message", terms));
                    out.append("phrase message=").append(String.join(",", terms))
                       .append(" count=").append(s.count(q)).append('\n');
                }
            }
        }
        System.out.print(out);
    }
}
