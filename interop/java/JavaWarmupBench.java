import java.nio.file.*;
import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.*;
import org.apache.lucene.analysis.core.WhitespaceAnalyzer;
import org.apache.lucene.document.*;
import org.apache.lucene.index.*;
import org.apache.lucene.store.*;

/**
 * Java writer benchmark with in-process JIT warmup (Lucene 9.12.3).
 * Same document shape and corpus generator as JavaLuceneBench (seed-compatible),
 * but runs several indexing rounds inside ONE JVM so HotSpot can tier-compile
 * the hot paths before the final measured round.
 *
 * Usage: JavaWarmupBench <indexRoot> <numDocs> <docBytes> <threads> <seed> [warmupRounds]
 * Each round indexes into a fresh directory <indexRoot>/round-<i> and prints:
 *   BENCH round=<i> elapsed_ms=<m> docs_per_sec=<d> mb_per_sec=<b> indexed_bytes=<n>
 */
public class JavaWarmupBench {
    static final class XorShift {
        long s;
        XorShift(long seed) { s = seed == 0 ? 0x9E3779B97F4A7C15L : seed; }
        long next() {
            s ^= s >>> 12; s ^= s << 25; s ^= s >>> 27;
            return s * 0x2545F4914F6CDD1DL;
        }
        int nextInt(int n) { return (int) Long.remainderUnsigned(next(), n); }
    }

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

    static String genMessage(XorShift rng, int targetBytes) {
        StringBuilder sb = new StringBuilder(targetBytes + 16);
        while (sb.length() < targetBytes) {
            sb.append(VOCAB[rng.nextInt(VOCAB.length)]).append(' ');
        }
        sb.setLength(targetBytes);
        return sb.toString();
    }

    static void round(Path indexDir, int numDocs, int docBytes, int threads, long seed, int roundNo) throws Exception {
        try (Directory dir = FSDirectory.open(indexDir)) {
            IndexWriterConfig cfg = new IndexWriterConfig(new WhitespaceAnalyzer())
                .setOpenMode(IndexWriterConfig.OpenMode.CREATE)
                .setUseCompoundFile(false)
                .setRAMBufferSizeMB(256);
            try (IndexWriter w = new IndexWriter(dir, cfg)) {
                AtomicLong indexedBytes = new AtomicLong(0);
                int perThread = numDocs / threads;
                ExecutorService pool = Executors.newFixedThreadPool(threads);
                long t0 = System.nanoTime();
                List<Future<?>> futures = new ArrayList<>();
                for (int t = 0; t < threads; t++) {
                    final int tid = t;
                    futures.add(pool.submit(() -> {
                        XorShift rng = new XorShift(seed + tid * 0x9E3779B97F4A7C15L);
                        FieldType ft = new FieldType();
                        ft.setIndexOptions(IndexOptions.DOCS_AND_FREQS);
                        ft.setOmitNorms(true);
                        ft.setStored(true);
                        ft.freeze();
                        try {
                            for (int i = 0; i < perThread; i++) {
                                String msg = genMessage(rng, docBytes);
                                Document doc = new Document();
                                doc.add(new Field("message", msg, ft));
                                w.addDocument(doc);
                                indexedBytes.addAndGet(msg.length());
                            }
                        } catch (Exception e) { throw new RuntimeException(e); }
                    }));
                }
                for (Future<?> f : futures) f.get();
                pool.shutdown();
                w.commit();
                long elapsedMs = (System.nanoTime() - t0) / 1_000_000;
                double docsPerSec = numDocs * 1000.0 / Math.max(1, elapsedMs);
                double mbPerSec = indexedBytes.get() / 1024.0 / 1024.0 / (Math.max(1, elapsedMs) / 1000.0);
                System.out.printf(Locale.ROOT,
                    "BENCH round=%d elapsed_ms=%d docs_per_sec=%.0f mb_per_sec=%.1f indexed_bytes=%d%n",
                    roundNo, elapsedMs, docsPerSec, mbPerSec, indexedBytes.get());
            }
        }
    }

    public static void main(String[] args) throws Exception {
        Path indexRoot = Paths.get(args[0]);
        int numDocs = Integer.parseInt(args[1]);
        int docBytes = Integer.parseInt(args[2]);
        int threads = Integer.parseInt(args[3]);
        long seed = Long.parseLong(args[4]);
        int warmupRounds = args.length > 5 ? Integer.parseInt(args[5]) : 2;

        for (int i = 0; i <= warmupRounds; i++) {
            Path dir = indexRoot.resolve("round-" + i);
            Files.createDirectories(dir);
            round(dir, numDocs, docBytes, threads, seed, i);
        }
    }
}
