import java.nio.file.Path;
import org.apache.lucene.document.Document;
import org.apache.lucene.index.CheckIndex;
import org.apache.lucene.index.DirectoryReader;
import org.apache.lucene.store.FSDirectory;

/**
 * Verifies the Rust-written segment in /tmp/rustlucene-m1a:
 * reads every stored "body" field via DirectoryReader and compares with the
 * expected values, then runs CheckIndex (non-zero exit on failure).
 */
public class VerifyStored {
  private static final String[] EXPECTED = {
    "the quick brown fox jumps over the lazy dog",
    "lucene stored fields best speed lz4 chunk format",
    "rust writes, java reads: ロシア語ではないが UTF-8 も OK — Grüße!",
  };

  public static void main(String[] args) throws Exception {
    Path indexPath = Path.of("/tmp/rustlucene-m1a");
    try (FSDirectory dir = FSDirectory.open(indexPath);
        DirectoryReader reader = DirectoryReader.open(dir)) {
      if (reader.maxDoc() != EXPECTED.length) {
        throw new IllegalStateException(
            "maxDoc=" + reader.maxDoc() + ", expected " + EXPECTED.length);
      }
      for (int i = 0; i < EXPECTED.length; i++) {
        Document doc = reader.storedFields().document(i);
        String body = doc.get("body");
        if (!EXPECTED[i].equals(body)) {
          throw new IllegalStateException(
              "doc " + i + " mismatch:\n  expected: " + EXPECTED[i] + "\n  actual:   " + body);
        }
        System.out.println("doc " + i + " OK: " + body);
      }
      System.out.println("stored fields: all " + EXPECTED.length + " docs match");
    }

    // CheckIndex: main() itself System.exits non-zero on failure (:4075-4077).
    System.out.println("--- CheckIndex ---");
    CheckIndex.main(new String[] {indexPath.toString(), "-verbose"});
    System.out.println("CheckIndex OK");
  }
}
