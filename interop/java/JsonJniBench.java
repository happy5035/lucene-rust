import java.io.BufferedReader;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.util.*;

/**
 * JNI JSON batch benchmark: reads a JSONL file (e.g. from
 * `rustlucene-cli jsongen`), packs lines into batches and hands the raw bytes
 * to RustIndexWriter.addJsonBatch — one JNI crossing per batch, with all
 * parsing/binding/coercion inside Rust.
 *
 * Usage: JsonJniBench <jsonlFile> <indexDir> <schemaSpec> [batchSize]
 * Output: INDEXED docs=.. elapsed_ms=.. docs_per_sec=.. batch_failed=..
 */
public class JsonJniBench {
    public static void main(String[] args) throws Exception {
        if (args.length < 3) {
            System.err.println("usage: JsonJniBench <jsonlFile> <indexDir> <schemaSpec> [batchSize]");
            System.exit(2);
        }
        Path input = Paths.get(args[0]);
        Path indexDir = Paths.get(args[1]);
        String schemaSpec = args[2];
        int batchSize = args.length > 3 ? Integer.parseInt(args[3]) : 1000;

        long docs = 0, failed = 0;
        long t0 = System.nanoTime();
        try (RustIndexWriter w = new RustIndexWriter(indexDir.toString(), schemaSpec);
             BufferedReader br = Files.newBufferedReader(input, StandardCharsets.UTF_8)) {
            ArrayList<byte[]> batch = new ArrayList<>(batchSize);
            String line;
            while (true) {
                line = br.readLine();
                if (line != null && !line.isBlank()) {
                    batch.add(line.getBytes(StandardCharsets.UTF_8));
                }
                if (batch.size() == batchSize || (line == null && !batch.isEmpty())) {
                    long r = w.addJsonBatch(batch.toArray(new byte[0][]));
                    docs += r >>> 32;
                    failed += r & 0xffffffffL;
                    batch.clear();
                }
                if (line == null) break;
            }
            w.commit();
        }
        long ms = Math.max(1, (System.nanoTime() - t0) / 1_000_000);
        System.out.printf(Locale.ROOT,
                "INDEXED docs=%d elapsed_ms=%d docs_per_sec=%.0f batch_failed=%d%n",
                docs, ms, docs * 1000.0 / ms, failed);
    }
}
