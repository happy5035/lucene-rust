import java.io.*;
import java.nio.file.*;
import java.util.*;
import org.apache.lucene.document.*;
import org.apache.lucene.index.*;
import org.apache.lucene.search.*;
import org.apache.lucene.store.*;

/**
 * Verifies a Rust-written index against a golden file produced by the Rust CLI.
 *
 * Golden file format (text, UTF-8):
 *   DOCS <numDocs>
 *   <docID>\t<field>=<value>[<US><field>=<value>...]       (one line per doc, docID order)
 *   TERMS <field> <numTerms>
 *   <term>\t<docID>,<docID>,...                            (sorted docIDs)
 *
 * Checks:
 *  1. DirectoryReader opens; numDocs matches.
 *  2. Stored fields of every doc match exactly.
 *  3. For every term: ConstantScoreQuery(TermQuery) returns exactly the golden docID set.
 */
public class VerifyIndex {
    public static void main(String[] args) throws Exception {
        Path indexDir = Paths.get(args[0]);
        Path golden = Paths.get(args[1]);

        List<String> lines = Files.readAllLines(golden);
        int li = 0;
        if (!lines.get(li).startsWith("DOCS ")) throw new IllegalStateException("bad golden header");
        int numDocs = Integer.parseInt(lines.get(li++).substring(5));

        Map<Integer, Map<String, String>> expectedStored = new HashMap<>();
        for (int d = 0; d < numDocs; d++, li++) {
            String[] parts = lines.get(li).split("\t", 2);
            int docId = Integer.parseInt(parts[0]);
            Map<String, String> fields = new LinkedHashMap<>();
            if (parts.length > 1) {
                for (String fv : parts[1].split("\u001F")) {
                    int eq = fv.indexOf('=');
                    fields.put(fv.substring(0, eq), fv.substring(eq + 1));
                }
            }
            expectedStored.put(docId, fields);
        }

        String[] th = lines.get(li++).split(" ");
        if (!th[0].equals("TERMS")) throw new IllegalStateException("bad TERMS header");
        String termField = th[1];
        int numTerms = Integer.parseInt(th[2]);
        Map<String, int[]> expectedPostings = new LinkedHashMap<>();
        for (int t = 0; t < numTerms; t++, li++) {
            String[] parts = lines.get(li).split("\t");
            String[] ids = parts[1].isEmpty() ? new String[0] : parts[1].split(",");
            int[] docs = Arrays.stream(ids).mapToInt(Integer::parseInt).toArray();
            expectedPostings.put(parts[0], docs);
        }

        try (Directory dir = FSDirectory.open(indexDir);
             IndexReader reader = DirectoryReader.open(dir)) {
            IndexSearcher searcher = new IndexSearcher(reader);
            int errors = 0;

            if (reader.numDocs() != numDocs) {
                System.out.println("FAIL numDocs: expected " + numDocs + " got " + reader.numDocs());
                errors++;
            }

            for (int d = 0; d < numDocs; d++) {
                Document doc = reader.storedFields().document(d);
                Map<String, String> exp = expectedStored.get(d);
                for (Map.Entry<String, String> e : exp.entrySet()) {
                    String actual = doc.get(e.getKey());
                    if (!e.getValue().equals(actual)) {
                        System.out.println("FAIL stored doc " + d + " field " + e.getKey()
                            + ": expected [" + e.getValue() + "] got [" + actual + "]");
                        if (++errors > 20) { summary(errors); }
                    }
                }
            }

            for (Map.Entry<String, int[]> e : expectedPostings.entrySet()) {
                Query q = new ConstantScoreQuery(new TermQuery(new Term(termField, e.getKey())));
                TopDocs td = searcher.search(q, numDocs + 1);
                int[] got = Arrays.stream(td.scoreDocs).mapToInt(sd -> sd.doc).sorted().toArray();
                if (!Arrays.equals(e.getValue(), got)) {
                    System.out.println("FAIL term [" + e.getKey() + "]: expected "
                        + Arrays.toString(e.getValue()) + " got " + Arrays.toString(got));
                    if (++errors > 20) { summary(errors); }
                }
            }

            summary(errors);
        }
    }

    private static void summary(int errors) {
        if (errors == 0) {
            System.out.println("VERIFY_OK");
            System.exit(0);
        } else {
            System.out.println("VERIFY_FAILED errors=" + errors);
            System.exit(1);
        }
    }
}
