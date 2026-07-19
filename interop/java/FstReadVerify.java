import java.nio.file.*;
import java.util.*;
import org.apache.lucene.util.BytesRef;
import org.apache.lucene.util.fst.*;

/**
 * Reads back an FST dumped by the Rust FstCompiler (examples/dump_fst.rs) with
 * the stock Java FST reader and verifies the full mapping.
 */
public class FstReadVerify {
    public static void main(String[] args) throws Exception {
        FST<BytesRef> fst = FST.read(Paths.get("/tmp/fst-cross.bin"), ByteSequenceOutputs.getSingleton());
        List<String> lines = Files.readAllLines(Paths.get("/tmp/fst-cross.txt"));
        int errors = 0, checked = 0;
        for (String line : lines) {
            String[] parts = line.split("\t", -1);
            byte[] input = unhex(parts[0]);
            byte[] expected = unhex(parts[1]);
            BytesRef got = Util.get(fst, new BytesRef(input));
            checked++;
            if (got == null || !Arrays.equals(expected, Arrays.copyOfRange(got.bytes, got.offset, got.offset + got.length))) {
                System.out.println("MISMATCH input=" + parts[0] + " expected=" + parts[1]
                    + " got=" + (got == null ? "null" : hex(got)));
                errors++;
            }
        }
        // Negative lookups
        for (String missing : new String[] {"aa", "abce", "term0499x", "zzz"}) {
            if (Util.get(fst, new BytesRef(missing)) != null) {
                System.out.println("FALSE-POSITIVE: " + missing);
                errors++;
            }
        }
        System.out.println(errors == 0 ? "FST_VERIFY_OK checked=" + checked : "FST_VERIFY_FAILED errors=" + errors);
        System.exit(errors == 0 ? 0 : 1);
    }

    static byte[] unhex(String s) {
        byte[] out = new byte[s.length() / 2];
        for (int i = 0; i < out.length; i++) out[i] = (byte) Integer.parseInt(s.substring(2 * i, 2 * i + 2), 16);
        return out;
    }

    static String hex(BytesRef b) {
        StringBuilder sb = new StringBuilder();
        for (int i = b.offset; i < b.offset + b.length; i++) sb.append(String.format("%02x", b.bytes[i]));
        return sb.toString();
    }
}
