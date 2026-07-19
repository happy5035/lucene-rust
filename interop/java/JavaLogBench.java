import java.nio.file.*;
import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.*;
import org.apache.lucene.analysis.core.WhitespaceAnalyzer;
import org.apache.lucene.document.*;
import org.apache.lucene.index.*;
import org.apache.lucene.store.*;
import org.apache.lucene.util.BytesRef;

/**
 * Java baseline writer for the M2 log scenario (Lucene 9.12.3). Field shape
 * mirrors the Rust `log_schema` exactly:
 *   timestamp: LongPoint + NumericDocValues + StoredField
 *   level:     StringField (stored) + SortedDocValues
 *   trace_id:  StringField (stored)
 *   message:   text, DOCS_AND_FREQS[+POSITIONS] + omitNorms, stored
 *   latency_ms / bytes_sent / status: NumericDocValues
 *
 * The corpus generator is byte-identical with rustlucene-cli (same xorshift64*
 * stream and per-document call order: ts jitter, level, trace hi/lo, message,
 * latency, bytes, status).
 *
 * Usage: JavaLogBench <indexDir> <numDocs> <threads> <seed> [--positions]
 */
public class JavaLogBench {
    static final class XorShift {
        long s;
        XorShift(long seed) { s = seed == 0 ? 0x9E3779B97F4A7C15L : seed; }
        long next() {
            s ^= s >>> 12; s ^= s << 25; s ^= s >>> 27;
            return s * 0x2545F4914F6CDD1DL;
        }
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

    static String genMessage(XorShift rng, int targetBytes) {
        StringBuilder sb = new StringBuilder(targetBytes + 16);
        while (sb.length() < targetBytes) {
            sb.append(VOCAB[rng.nextInt(VOCAB.length)]).append(' ');
        }
        sb.setLength(targetBytes);
        return sb.toString();
    }

    public static void main(String[] args) throws Exception {
        Path indexDir = Paths.get(args[0]);
        int numDocs = Integer.parseInt(args[1]);
        int threads = Integer.parseInt(args[2]);
        long seed = Long.parseLong(args[3]);
        boolean positions = false;
        boolean sparse = false;
        boolean bigdict = false;
        for (String a : args) {
            if (a.equals("--positions")) positions = true;
            if (a.equals("--sparse")) sparse = true;
            if (a.equals("--bigdict")) bigdict = true;
        }
        final boolean sparseF = sparse;
        final boolean bigdictF = bigdict;

        final FieldType msgType = new FieldType();
        msgType.setIndexOptions(positions
            ? IndexOptions.DOCS_AND_FREQS_AND_POSITIONS
            : IndexOptions.DOCS_AND_FREQS);
        msgType.setOmitNorms(true);
        msgType.setStored(true);
        msgType.freeze();

        try (Directory dir = FSDirectory.open(indexDir)) {
            IndexWriterConfig cfg = new IndexWriterConfig(new WhitespaceAnalyzer())
                .setOpenMode(IndexWriterConfig.OpenMode.CREATE)
                .setUseCompoundFile(false)
                .setRAMBufferSizeMB(256);
            try (IndexWriter w = new IndexWriter(dir, cfg)) {
                int perThread = numDocs / threads;
                ExecutorService pool = Executors.newFixedThreadPool(threads);
                long t0 = System.nanoTime();
                List<Future<long[]>> futures = new ArrayList<>();
                for (int t = 0; t < threads; t++) {
                    final int tid = t;
                    futures.add(pool.submit(() -> {
                        XorShift rng = new XorShift(seed + tid * 0x9E3779B97F4A7C15L);
                        long[] samples = new long[perThread / 16 + 2];
                        int nSamples = 0;
                        try {
                            for (int docId = 0; docId < perThread; docId++) {
                                long ts = TS_BASE + docId * 1000L + rng.nextInt(1000);
                                String level = LEVELS[rng.nextInt(5)];
                                String traceId = String.format("%016x%016x", rng.next(), rng.next());
                                String message = genMessage(rng, 200);
                                long latency = rng.nextInt(10_000);
                                long bytes = rng.nextInt(1_000_000);
                                long status = STATUSES[rng.nextInt(5)];

                                Document doc = new Document();
                                doc.add(new LongPoint("timestamp", ts));
                                doc.add(new NumericDocValuesField("timestamp", ts));
                                doc.add(new StoredField("timestamp", ts));
                                if (!sparseF || docId % 13 != 0) {
                                    doc.add(new StringField("level", level, Field.Store.YES));
                                    doc.add(new SortedDocValuesField("level", new BytesRef(level)));
                                }
                                doc.add(new StringField("trace_id", traceId, Field.Store.YES));
                                if (bigdictF) {
                                    doc.add(new SortedDocValuesField("trace_id_sdv", new BytesRef(traceId)));
                                }
                                doc.add(new Field("message", message, msgType));
                                if (!sparseF || docId % 7 != 0) {
                                    doc.add(new NumericDocValuesField("latency_ms", latency));
                                }
                                if (!sparseF || docId % 11 != 0) {
                                    doc.add(new NumericDocValuesField("bytes_sent", bytes));
                                }
                                if (!sparseF || docId % 17 == 0) {
                                    doc.add(new NumericDocValuesField("status", status));
                                }
                                long tAdd = System.nanoTime();
                                w.addDocument(doc);
                                if (docId % 16 == 0) {
                                    samples[nSamples++] = System.nanoTime() - tAdd;
                                }
                            }
                        } catch (Exception e) { throw new RuntimeException(e); }
                        return Arrays.copyOf(samples, nSamples);
                    }));
                }
                List<Long> all = new ArrayList<>();
                for (Future<long[]> f : futures) {
                    for (long v : f.get()) all.add(v);
                }
                pool.shutdown();
                w.commit();
                long elapsedMs = (System.nanoTime() - t0) / 1_000_000;
                Collections.sort(all);
                double p50us = all.isEmpty() ? 0 : all.get(all.size() / 2) / 1000.0;
                double p99us = all.isEmpty() ? 0 : all.get(all.size() * 99 / 100) / 1000.0;
                double docsPerSec = numDocs * 1000.0 / Math.max(1, elapsedMs);
                long indexedBytes = (long) numDocs * 240; // same approximation as the Rust side
                double mbPerSec = indexedBytes / 1024.0 / 1024.0 / (Math.max(1, elapsedMs) / 1000.0);
                System.out.printf(Locale.ROOT,
                    "BENCH elapsed_ms=%d docs_per_sec=%.0f mb_per_sec=%.1f indexed_bytes=%d add_p50_us=%.1f add_p99_us=%.1f%n",
                    elapsedMs, docsPerSec, mbPerSec, indexedBytes, p50us, p99us);
            }
        }
    }
}
