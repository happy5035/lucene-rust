import org.apache.lucene.index.*;
import org.apache.lucene.store.FSDirectory;
import java.nio.file.Paths;

// Verifies an index-sorted segment is physically ordered by the named
// NumericDocValues field: iterating docIDs in order must yield a
// non-decreasing value sequence (Lucene index sort guarantee).
public class VerifyIndexSort {
    public static void main(String[] args) throws Exception {
        String dir = args[0];
        String field = args[1];
        try (DirectoryReader r = DirectoryReader.open(FSDirectory.open(Paths.get(dir)))) {
            LeafReader leaf = r.leaves().get(0).reader();
            NumericDocValues dv = leaf.getNumericDocValues(field);
            if (dv == null) {
                System.out.println("NO_DV field=" + field);
                System.exit(2);
            }
            int max = leaf.maxDoc();
            long prev = Long.MIN_VALUE;
            int violations = 0;
            int missing = 0;
            for (int d = 0; d < max; d++) {
                if (!dv.advanceExact(d)) {
                    missing++;
                    continue;
                }
                long v = dv.longValue();
                if (v < prev) {
                    violations++;
                    if (violations <= 5) {
                        System.out.println("VIOLATION doc=" + d + " v=" + v + " prev=" + prev);
                    }
                }
                prev = v;
            }
            System.out.println("VERIFY field=" + field + " maxDoc=" + max
                    + " missing=" + missing + " violations=" + violations);
            System.out.println(violations == 0 ? "SORTED_OK" : "SORTED_FAIL");
        }
    }
}
