import java.nio.file.*;
import java.util.*;

/**
 * JNI smoke test + benchmark: writes the M2 log corpus through RustIndexWriter
 * (Rust engine over JNI), then prints the standard BENCH line. The index can
 * be checked with CheckIndex / VerifyLogIndex like any other.
 *
 * Usage: JniLogBench <indexDir> <numDocs> <seed> [--positions]
 */
public class JniLogBench {
    // Same generator as JavaLogBench (byte-identical corpora).
    static final class XorShift {
        long s;
        XorShift(long seed) { s = seed == 0 ? 0x9E3779B97F4A7C15L : seed; }
        long next() { s ^= s >>> 12; s ^= s << 25; s ^= s >>> 27; return s * 0x2545F4914F6CDD1DL; }
        int nextInt(int n) { return (int) Long.remainderUnsigned(next(), n); }
    }
    static final String[] LEVELS = {"INFO","WARN","ERROR","DEBUG","TRACE"};
    static final long[] STATUSES = {200, 200, 200, 404, 500};
    static final long TS_BASE = 1_700_000_000_000L;
    static final String[] VOCAB;
    static {
        List<String> v = new ArrayList<>();
        String[] levels = {"INFO","WARN","ERROR","DEBUG","TRACE"};
        String[] words = ("connection timeout retry backoff socket buffer stream packet request response "
            + "client server upstream downstream latency throughput commit rollback segment flush merge "
            + "index query filter cache eviction compaction snapshot replica shard leader follower election "
            + "heartbeat protocol handshake encrypt decrypt token session expire renew validate schema "
            + "migrate upgrade downgrade rollback checkpoint journal wal fsync sync async batch queue").split(" ");
        for (String w : words) for (int i = 0; i < 40; i++) v.add(w + i);
        Collections.addAll(v, levels);
        VOCAB = v.toArray(new String[0]);
    }

    public static void main(String[] args) throws Exception {
        Path indexDir = Paths.get(args[0]);
        int numDocs = Integer.parseInt(args[1]);
        long seed = Long.parseLong(args[2]);
        boolean positions = args.length > 3 && args[3].equals("--positions");

        String schema = "timestamp:longpoint+numericdv+stored"
            + ",level:keyword+sorteddv"
            + ",trace_id:keyword"
            + ",message:text" + (positions ? "+positions" : "")
            + ",latency_ms:numericdv,bytes_sent:numericdv,status:numericdv";

        XorShift rng = new XorShift(seed);
        long t0 = System.nanoTime();
        try (RustIndexWriter w = new RustIndexWriter(indexDir.toString(), schema)) {
            for (int docId = 0; docId < numDocs; docId++) {
                long ts = TS_BASE + docId * 1000L + rng.nextInt(1000);
                String level = LEVELS[rng.nextInt(5)];
                String traceId = String.format("%016x%016x", rng.next(), rng.next());
                StringBuilder sb = new StringBuilder(216);
                while (sb.length() < 200) sb.append(VOCAB[rng.nextInt(VOCAB.length)]).append(' ');
                sb.setLength(200);
                long latency = rng.nextInt(10_000);
                long bytes = rng.nextInt(1_000_000);
                long status = STATUSES[rng.nextInt(5)];

                w.beginDocument();
                w.addLong("timestamp", ts);
                w.addKeyword("level", level);
                w.addKeyword("trace_id", traceId);
                w.addText("message", sb.toString());
                w.addLong("latency_ms", latency);
                w.addLong("bytes_sent", bytes);
                w.addLong("status", status);
                w.endDocument();
            }
            w.commit();
        }
        long elapsedMs = (System.nanoTime() - t0) / 1_000_000;
        double docsPerSec = numDocs * 1000.0 / Math.max(1, elapsedMs);
        System.out.printf(Locale.ROOT, "BENCH elapsed_ms=%d docs_per_sec=%.0f%n", elapsedMs, docsPerSec);
    }
}
