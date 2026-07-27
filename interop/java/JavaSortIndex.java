import org.apache.lucene.analysis.core.WhitespaceAnalyzer;
import org.apache.lucene.document.*;
import org.apache.lucene.index.*;
import org.apache.lucene.search.Sort;
import org.apache.lucene.search.SortField;
import org.apache.lucene.store.FSDirectory;
import java.nio.file.Paths;

// Writes a small index with IndexWriterConfig.setIndexSort so the Rust read
// path can be verified against a genuine Java-written index sort (Phase B).
// Usage: JavaSortIndex <indexDir> <numDocs>
public class JavaSortIndex {
    public static void main(String[] args) throws Exception {
        String dir = args[0];
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 100;
        IndexWriterConfig cfg = new IndexWriterConfig(new WhitespaceAnalyzer());
        cfg.setIndexSort(new Sort(new SortField("timestamp", SortField.Type.LONG)));
        cfg.setOpenMode(IndexWriterConfig.OpenMode.CREATE);
        cfg.setUseCompoundFile(false); // Rust read path has no CFS reader
        try (IndexWriter w = new IndexWriter(FSDirectory.open(Paths.get(dir)), cfg)) {
            // insert out of order so the sort actually reorders
            for (int i = 0; i < n; i++) {
                long ts = ((long) (i * 7919)) % n; // scrambled
                Document d = new Document();
                d.add(new NumericDocValuesField("timestamp", ts));
                d.add(new StoredField("timestamp", ts));
                d.add(new StringField("tid", "t-" + i, Field.Store.YES));
                w.addDocument(d);
            }
            w.commit();
        }
        System.out.println("JAVA_SORT_INDEX_OK docs=" + n);
    }
}
