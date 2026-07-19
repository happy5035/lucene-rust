import java.nio.file.*;
import java.util.*;
import org.apache.lucene.index.*;
import org.apache.lucene.store.FSDirectory;
import org.apache.lucene.util.BytesRef;

/**
 * Compares two indexes term-by-term on one field (constant-score semantics:
 * postings only, no scoring). Samples a random ~10% of A's term dictionary
 * (seeded, deterministic) and checks that each sampled term's doc list —
 * docIDs in order, with per-doc freq — is identical in B. Also compares
 * maxDoc and dictionary sizes (guards against terms present only in B).
 *
 * Usage: CompareIndexes <indexDirA> <indexDirB> <field> [samplePct] [seed]
 * Prints COMPARE_PASS / COMPARE_FAIL; exit 0 on pass, 1 on fail.
 */
public class CompareIndexes {
    public static void main(String[] args) throws Exception {
        if (args.length < 3) {
            System.err.println("usage: CompareIndexes <indexDirA> <indexDirB> <field> [samplePct] [seed]");
            System.exit(2);
        }
        Path dirA = Paths.get(args[0]);
        Path dirB = Paths.get(args[1]);
        String field = args[2];
        double pct = args.length > 3 ? Double.parseDouble(args[3]) : 10.0;
        long seed = args.length > 4 ? Long.parseLong(args[4]) : 42;

        try (DirectoryReader a = DirectoryReader.open(FSDirectory.open(dirA));
             DirectoryReader b = DirectoryReader.open(FSDirectory.open(dirB))) {
            int maxA = a.maxDoc(), maxB = b.maxDoc();
            Terms termsA = MultiTerms.getTerms(a, field);
            Terms termsB = MultiTerms.getTerms(b, field);
            // NOTE: MultiTerms.size() always returns -1 (see MultiTerms.java) —
            // it only yields a value for single-segment readers. Dictionary
            // sizes must therefore be compared by enumeration (A is fully
            // enumerated below anyway; B is enumerated only if size() is -1).
            long sizeB = termsB == null ? 0 : termsB.size();
            if (sizeB == -1 && termsB != null) {
                sizeB = 0;
                TermsEnum c = termsB.iterator();
                while (c.next() != null) {
                    sizeB++;
                }
            }

            long total = 0, sampled = 0, mismatches = 0;
            Random rng = new Random(seed);
            if (termsA != null) {
                TermsEnum teA = termsA.iterator();
                TermsEnum teB = termsB == null ? null : termsB.iterator();
                PostingsEnum pa = null, pb = null;
                BytesRef term;
                while ((term = teA.next()) != null) {
                    total++;
                    if (rng.nextDouble() * 100.0 >= pct) {
                        continue;
                    }
                    sampled++;
                    pa = teA.postings(pa, PostingsEnum.FREQS);
                    pb = (teB != null && teB.seekExact(term))
                            ? teB.postings(pb, PostingsEnum.FREQS)
                            : null;
                    String diff = diffPostings(pa, pb);
                    if (diff != null) {
                        mismatches++;
                        if (mismatches <= 10) {
                            System.out.println("MISMATCH term=" + term.utf8ToString() + " " + diff);
                        }
                    }
                }
            }

            boolean pass = maxA == maxB && total == sizeB && mismatches == 0;
            System.out.printf(Locale.ROOT,
                    "COMPARE field=%s maxdoc_a=%d maxdoc_b=%d terms_a=%d terms_b=%d sampled=%d mismatches=%d%n",
                    field, maxA, maxB, total, sizeB, sampled, mismatches);
            System.out.println(pass ? "COMPARE_PASS" : "COMPARE_FAIL");
            System.exit(pass ? 0 : 1);
        }
    }

    /** Returns null if both postings sequences are identical, else a diff description. */
    private static String diffPostings(PostingsEnum a, PostingsEnum b) throws Exception {
        int pos = 0;
        while (true) {
            int da = a == null ? -1 : a.nextDoc();
            int db = b == null ? -1 : b.nextDoc();
            if (da == PostingsEnum.NO_MORE_DOCS && db == PostingsEnum.NO_MORE_DOCS) {
                return null;
            }
            if (da != db) {
                return "at posting #" + pos + ": doc_a=" + da + " doc_b=" + db;
            }
            int fa = a.freq(), fb = b.freq();
            if (fa != fb) {
                return "at doc " + da + ": freq_a=" + fa + " freq_b=" + fb;
            }
            pos++;
        }
    }
}
