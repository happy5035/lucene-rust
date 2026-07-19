import java.nio.file.*;
import java.util.*;
import org.apache.lucene.document.Document;
import org.apache.lucene.index.*;
import org.apache.lucene.search.*;
import org.apache.lucene.store.*;

/**
 * Searches an index written by the Rust writer (rustlucene-cli index ...).
 *
 * All queries are wrapped in ConstantScoreQuery — the Rust side writes
 * DOCS_AND_FREQS (+positions optional) with omitNorms, no scoring is used.
 *
 * Usage:
 *   SearchIndex <indexDir> term   <field> <word> [maxHits]
 *   SearchIndex <indexDir> and    <field> <word1> <word2> ... [maxHits]
 *   SearchIndex <indexDir> phrase <field> <word1> <word2> ... [maxHits]
 *
 * "and" requires all terms (BooleanQuery MUST). "phrase" requires the field
 * to have been indexed with positions (--positions). A trailing numeric
 * argument sets maxHits (default 10); maxHits=0 prints the hit count only.
 */
public class SearchIndex {
    public static void main(String[] args) throws Exception {
        if (args.length < 4) {
            System.err.println("usage: SearchIndex <indexDir> <term|and|phrase|count> <field> <word...> [maxHits]");
            System.exit(2);
        }
        Path indexDir = Paths.get(args[0]);
        String mode = args[1];
        String field = args[2];

        List<String> words = new ArrayList<>(Arrays.asList(args).subList(3, args.length));
        int maxHits = 10;
        if (!words.isEmpty() && words.get(words.size() - 1).matches("\\d+")) {
            maxHits = Integer.parseInt(words.remove(words.size() - 1));
        }
        if (words.isEmpty()) {
            System.err.println("at least one word is required");
            System.exit(2);
        }

        Query query;
        switch (mode) {
            case "term":
                query = new ConstantScoreQuery(new TermQuery(new Term(field, words.get(0))));
                break;
            case "and": {
                BooleanQuery.Builder b = new BooleanQuery.Builder();
                for (String w : words) {
                    b.add(new ConstantScoreQuery(new TermQuery(new Term(field, w))), BooleanClause.Occur.MUST);
                }
                query = new ConstantScoreQuery(b.build());
                break;
            }
            case "phrase":
                query = new ConstantScoreQuery(new PhraseQuery(field, words.toArray(new String[0])));
                break;
            default:
                System.err.println("unknown mode: " + mode);
                System.exit(2);
                return;
        }

        try (DirectoryReader r = DirectoryReader.open(FSDirectory.open(indexDir))) {
            IndexSearcher searcher = new IndexSearcher(r);
            if (maxHits <= 0) {
                System.out.println("COUNT " + searcher.count(query));
                return;
            }
            TopDocs top = searcher.search(query, maxHits);
            System.out.println("HITS total=" + top.totalHits.value
                    + (top.totalHits.relation == TotalHits.Relation.EQUAL_TO ? "" : "+")
                    + " shown=" + top.scoreDocs.length);
            StoredFields stored = r.storedFields();
            for (ScoreDoc sd : top.scoreDocs) {
                Document d = stored.document(sd.doc);
                System.out.println("doc=" + sd.doc
                        + " source=" + d.get("source")
                        + " line=" + d.get("line")
                        + " message=" + d.get("message"));
            }
        }
    }
}
