import java.io.*;
import java.nio.file.*;
import java.util.*;
import org.apache.lucene.document.*;
import org.apache.lucene.index.*;
import org.apache.lucene.store.*;
import org.apache.lucene.search.*;
import org.apache.lucene.util.*;

/**
 * Java Lucene 9.12.3 DocValues benchmark — mirrors bench_docvalues.rs scenarios.
 * Read benchmarks run each scenario in a forked JVM to defeat C2 DCE.
 *
 * Usage: java -cp lib/*:. JavaDocValuesBench [numDocs]
 */
public class JavaDocValuesBench {

    static final int WARMUP = 5;
    static final int ITERS = 5;

    static int numDocs = 500_000;

    public static void main(String[] args) throws Exception {
        // If invoked with "scan" as first arg, run a single scan scenario (forked mode)
        if (args.length >= 2 && args[0].equals("scan")) {
            runScanScenario(args[1], args.length > 2 ? Integer.parseInt(args[2]) : 500_000);
            return;
        }

        if (args.length > 0) numDocs = Integer.parseInt(args[0]);

        System.out.println("╔══════════════════════════════════════════════════════════════════════╗");
        System.out.printf("║     Java Lucene 9.12.3 DocValues Benchmark — %9d docs          ║%n", numDocs);
        System.out.println("╚══════════════════════════════════════════════════════════════════════╝");

        System.out.println("\n═══ 1. WRITE THROUGHPUT (docs/sec, includes flush) ═══\n");
        System.out.printf("%-40s %14s%n", "scenario", "docs/sec");
        System.out.println("─".repeat(56));

        benchWriteNumeric();
        for (int card : new int[]{10, 1_000, 100_000}) benchWriteSorted(card);
        for (int sz : new int[]{16, 256, 4096}) benchWriteBinary(sz);
        benchWriteWide(16);
        for (int density : new int[]{100, 50, 10}) benchWriteSparse(density);

        System.out.println("\n═══ 2. READ — NUMERIC (full-scan) ═══\n");
        System.out.printf("%-40s %14s %12s%n", "scenario", "docs/sec", "MB/s");
        System.out.println("─".repeat(68));
        for (int bits : new int[]{8, 32, 63}) {
            buildNumericIndex(Paths.get("/tmp/bench-jdv-r-num"), bits);
            forkScan("numeric", bits, "range=" + bits + "bit (bpv≈" + bits + ")", numDocs * 8.0);
            deleteDir(Paths.get("/tmp/bench-jdv-r-num"));
        }

        System.out.println("\n═══ 3. READ — SORTED (full-scan, ord→bytes) ═══\n");
        System.out.printf("%-40s %14s %12s%n", "scenario", "docs/sec", "MB/s");
        System.out.println("─".repeat(68));
        for (int card : new int[]{10, 1_000, 100_000}) {
            buildSortedIndex(Paths.get("/tmp/bench-jdv-r-sorted"), card);
            forkScan("sorted", card, "cardinality=" + card, numDocs * 13.0);
            deleteDir(Paths.get("/tmp/bench-jdv-r-sorted"));
        }

        System.out.println("\n═══ 4. READ — BINARY (full-scan) ═══\n");
        System.out.printf("%-40s %14s %12s%n", "scenario", "docs/sec", "MB/s");
        System.out.println("─".repeat(68));
        for (int sz : new int[]{16, 256, 4096}) {
            buildBinaryIndex(Paths.get("/tmp/bench-jdv-r-bin"), sz);
            forkScan("binary", sz, "payload=" + sz + "B", (double) numDocs * sz);
            deleteDir(Paths.get("/tmp/bench-jdv-r-bin"));
        }

        System.out.println("\n═══ 5. READ — SPARSE NUMERIC (IndexedDISI skip) ═══\n");
        System.out.printf("%-40s %14s%n", "scenario", "valued-docs/sec");
        System.out.println("─".repeat(56));
        for (int density : new int[]{100, 50, 10, 1}) {
            buildSparseIndex(Paths.get("/tmp/bench-jdv-r-sparse"), density);
            forkScan("sparse", density, density + "% density", 0);
            deleteDir(Paths.get("/tmp/bench-jdv-r-sparse"));
        }

        System.out.println("\n✓ done");
    }

    // ─── Forked scan runner ─────────────────────────────────────────────────

    static void forkScan(String type, int param, String label, double dataBytes) throws Exception {
        String cp = System.getProperty("java.class.path");
        ProcessBuilder pb = new ProcessBuilder(
            "java", "-cp", cp, "-Xmx4g",
            "JavaDocValuesBench", "scan", type + ":" + param, String.valueOf(numDocs)
        );
        pb.redirectErrorStream(true);
        Process p = pb.start();
        String output = new String(p.getInputStream().readAllBytes());
        p.waitFor();
        // Output format: "RESULT count=N elapsed_us=T"
        long count = 0, elapsedUs = 0;
        for (String line : output.split("\n")) {
            if (line.startsWith("RESULT")) {
                for (String tok : line.split(" ")) {
                    if (tok.startsWith("count=")) count = Long.parseLong(tok.substring(6));
                    if (tok.startsWith("elapsed_us=")) elapsedUs = Long.parseLong(tok.substring(11));
                }
            }
        }
        if (elapsedUs == 0) { System.out.printf("%-40s %14s %12s%n", label, "ERROR", "-"); return; }
        double secs = elapsedUs / 1e6;
        double docsPerSec = count / secs;
        if (type.equals("sparse")) {
            System.out.printf("%-40s %14.0f%n", label, docsPerSec);
        } else {
            double mbPerSec = dataBytes / secs / 1e6;
            System.out.printf("%-40s %14.0f %12.1f%n", label, docsPerSec, mbPerSec);
        }
    }

    static void runScanScenario(String spec, int nDocs) throws Exception {
        numDocs = nDocs;
        String[] parts = spec.split(":");
        String type = parts[0];
        int param = Integer.parseInt(parts[1]);

        Path path;
        switch (type) {
            case "numeric" -> path = Paths.get("/tmp/bench-jdv-r-num");
            case "sorted" -> path = Paths.get("/tmp/bench-jdv-r-sorted");
            case "binary" -> path = Paths.get("/tmp/bench-jdv-r-bin");
            case "sparse" -> path = Paths.get("/tmp/bench-jdv-r-sparse");
            default -> throw new IllegalArgumentException(type);
        }

        try (Directory dir = FSDirectory.open(path);
             DirectoryReader reader = DirectoryReader.open(dir)) {
            // warmup
            for (int wi = 0; wi < WARMUP; wi++) {
                long c = doScan(reader, type);
                if (c < 0) throw new IOException("scan failed");
            }
            // timed
            long best = Long.MAX_VALUE;
            long count = 0;
            for (int iter = 0; iter < ITERS; iter++) {
                long t0 = System.nanoTime();
                count = doScan(reader, type);
                long elapsed = (System.nanoTime() - t0) / 1000;
                if (elapsed < best) best = elapsed;
            }
            System.out.println("RESULT count=" + count + " elapsed_us=" + best);
        }
    }

    static long doScan(DirectoryReader reader, String type) throws IOException {
        long acc = 0;
        long count = 0;
        switch (type) {
            case "numeric", "sparse" -> {
                for (LeafReaderContext ctx : reader.leaves()) {
                    NumericDocValues ndv = ctx.reader().getNumericDocValues("val");
                    if (ndv == null) continue;
                    int d;
                    while ((d = ndv.nextDoc()) != DocIdSetIterator.NO_MORE_DOCS) {
                        acc += ndv.longValue();
                        count++;
                    }
                }
            }
            case "sorted" -> {
                for (LeafReaderContext ctx : reader.leaves()) {
                    SortedDocValues sdv = ctx.reader().getSortedDocValues("tag");
                    if (sdv == null) continue;
                    int d;
                    while ((d = sdv.nextDoc()) != DocIdSetIterator.NO_MORE_DOCS) {
                        BytesRef br = sdv.lookupOrd(sdv.ordValue());
                        acc += br.length + (br.length > 0 ? br.bytes[br.offset] : 0);
                        count++;
                    }
                }
            }
            case "binary" -> {
                for (LeafReaderContext ctx : reader.leaves()) {
                    BinaryDocValues bdv = ctx.reader().getBinaryDocValues("blob");
                    if (bdv == null) continue;
                    int d;
                    while ((d = bdv.nextDoc()) != DocIdSetIterator.NO_MORE_DOCS) {
                        BytesRef br = bdv.binaryValue();
                        acc += br.length + (br.length > 0 ? br.bytes[br.offset] : 0);
                        count++;
                    }
                }
            }
        }
        // Use acc to prevent DCE — print to stderr (captured but not parsed)
        if (acc == Long.MIN_VALUE) System.err.println("never");
        return count;
    }

    // ─── Write ──────────────────────────────────────────────────────────────

    static void benchWriteNumeric() throws Exception {
        Path path = Paths.get("/tmp/bench-jdv-w-num");
        deleteDir(path);
        long t0 = System.nanoTime();
        try (Directory dir = FSDirectory.open(path);
             IndexWriter w = new IndexWriter(dir, writerConfig())) {
            for (int i = 0; i < numDocs; i++) {
                Document doc = new Document();
                doc.add(new NumericDocValuesField("val", (long) i * 7 + 13));
                w.addDocument(doc);
            }
            w.commit();
        }
        double elapsed = (System.nanoTime() - t0) / 1e9;
        System.out.printf("%-40s %14.0f%n", "numeric (i64, full range)", numDocs / elapsed);
        deleteDir(path);
    }

    static void benchWriteSorted(int cardinality) throws Exception {
        Path path = Paths.get("/tmp/bench-jdv-w-sorted");
        deleteDir(path);
        String[] terms = new String[cardinality];
        for (int i = 0; i < cardinality; i++) terms[i] = String.format("term_%08d", i);
        long t0 = System.nanoTime();
        try (Directory dir = FSDirectory.open(path);
             IndexWriter w = new IndexWriter(dir, writerConfig())) {
            for (int i = 0; i < numDocs; i++) {
                Document doc = new Document();
                doc.add(new SortedDocValuesField("tag", new BytesRef(terms[i % cardinality])));
                w.addDocument(doc);
            }
            w.commit();
        }
        double elapsed = (System.nanoTime() - t0) / 1e9;
        System.out.printf("%-40s %14.0f%n", "sorted (cardinality=" + cardinality + ")", numDocs / elapsed);
        deleteDir(path);
    }

    static void benchWriteBinary(int payloadSize) throws Exception {
        Path path = Paths.get("/tmp/bench-jdv-w-bin");
        deleteDir(path);
        byte[][] payloads = new byte[64][payloadSize];
        for (int k = 0; k < 64; k++)
            for (int j = 0; j < payloadSize; j++)
                payloads[k][j] = (byte) ((k * payloadSize + j) % 251);
        long t0 = System.nanoTime();
        try (Directory dir = FSDirectory.open(path);
             IndexWriter w = new IndexWriter(dir, writerConfig())) {
            for (int i = 0; i < numDocs; i++) {
                Document doc = new Document();
                doc.add(new BinaryDocValuesField("blob", new BytesRef(payloads[i % 64])));
                w.addDocument(doc);
            }
            w.commit();
        }
        double elapsed = (System.nanoTime() - t0) / 1e9;
        System.out.printf("%-40s %14.0f%n", "binary (payload=" + payloadSize + "B)", numDocs / elapsed);
        deleteDir(path);
    }

    static void benchWriteWide(int numFields) throws Exception {
        Path path = Paths.get("/tmp/bench-jdv-w-wide");
        deleteDir(path);
        String[] terms = new String[1000];
        for (int i = 0; i < 1000; i++) terms[i] = String.format("t%04d", i);
        byte[] blob = new byte[64];
        for (int j = 0; j < 64; j++) blob[j] = (byte) (j % 251);
        long t0 = System.nanoTime();
        try (Directory dir = FSDirectory.open(path);
             IndexWriter w = new IndexWriter(dir, writerConfig())) {
            for (int i = 0; i < numDocs; i++) {
                Document doc = new Document();
                for (int f = 0; f < numFields; f++) {
                    switch (f % 3) {
                        case 0 -> doc.add(new NumericDocValuesField("num_" + f, (long) i * 3 + f));
                        case 1 -> doc.add(new SortedDocValuesField("str_" + f, new BytesRef(terms[(i + f) % 1000])));
                        default -> doc.add(new BinaryDocValuesField("bin_" + f, new BytesRef(blob)));
                    }
                }
                w.addDocument(doc);
            }
            w.commit();
        }
        double elapsed = (System.nanoTime() - t0) / 1e9;
        System.out.printf("%-40s %14.0f%n", "wide table (16 mixed columns)", numDocs / elapsed);
        deleteDir(path);
    }

    static void benchWriteSparse(int densityPct) throws Exception {
        Path path = Paths.get("/tmp/bench-jdv-w-sparse");
        deleteDir(path);
        long t0 = System.nanoTime();
        try (Directory dir = FSDirectory.open(path);
             IndexWriter w = new IndexWriter(dir, writerConfig())) {
            for (int i = 0; i < numDocs; i++) {
                Document doc = new Document();
                if (i % 100 < densityPct) {
                    doc.add(new NumericDocValuesField("val", (long) i));
                }
                w.addDocument(doc);
            }
            w.commit();
        }
        double elapsed = (System.nanoTime() - t0) / 1e9;
        System.out.printf("%-40s %14.0f%n", "numeric sparse (" + densityPct + "% density)", numDocs / elapsed);
        deleteDir(path);
    }

    // ─── Index Builders ─────────────────────────────────────────────────────

    static void buildNumericIndex(Path path, int rangeBits) throws Exception {
        deleteDir(path);
        long mask = rangeBits >= 63 ? Long.MAX_VALUE : (1L << rangeBits) - 1;
        try (Directory dir = FSDirectory.open(path);
             IndexWriter w = new IndexWriter(dir, writerConfig())) {
            for (int i = 0; i < numDocs; i++) {
                Document doc = new Document();
                doc.add(new NumericDocValuesField("val", ((long) i * 2654435761L) & mask));
                w.addDocument(doc);
            }
            w.commit();
        }
    }

    static void buildSortedIndex(Path path, int cardinality) throws Exception {
        deleteDir(path);
        String[] terms = new String[cardinality];
        for (int i = 0; i < cardinality; i++) terms[i] = String.format("term_%08d", i);
        try (Directory dir = FSDirectory.open(path);
             IndexWriter w = new IndexWriter(dir, writerConfig())) {
            for (int i = 0; i < numDocs; i++) {
                Document doc = new Document();
                doc.add(new SortedDocValuesField("tag", new BytesRef(terms[i % cardinality])));
                w.addDocument(doc);
            }
            w.commit();
        }
    }

    static void buildBinaryIndex(Path path, int payloadSize) throws Exception {
        deleteDir(path);
        byte[][] payloads = new byte[64][payloadSize];
        for (int k = 0; k < 64; k++)
            for (int j = 0; j < payloadSize; j++)
                payloads[k][j] = (byte) ((k * payloadSize + j) % 251);
        try (Directory dir = FSDirectory.open(path);
             IndexWriter w = new IndexWriter(dir, writerConfig())) {
            for (int i = 0; i < numDocs; i++) {
                Document doc = new Document();
                doc.add(new BinaryDocValuesField("blob", new BytesRef(payloads[i % 64])));
                w.addDocument(doc);
            }
            w.commit();
        }
    }

    static void buildSparseIndex(Path path, int densityPct) throws Exception {
        deleteDir(path);
        try (Directory dir = FSDirectory.open(path);
             IndexWriter w = new IndexWriter(dir, writerConfig())) {
            for (int i = 0; i < numDocs; i++) {
                Document doc = new Document();
                if (i % 100 < densityPct) {
                    doc.add(new NumericDocValuesField("val", (long) i));
                }
                w.addDocument(doc);
            }
            w.commit();
        }
    }

    // ─── Helpers ────────────────────────────────────────────────────────────

    static IndexWriterConfig writerConfig() {
        return new IndexWriterConfig()
            .setOpenMode(IndexWriterConfig.OpenMode.CREATE)
            .setUseCompoundFile(false)
            .setRAMBufferSizeMB(512);
    }

    static void deleteDir(Path path) throws IOException {
        if (Files.exists(path)) {
            try (var walk = Files.walk(path)) {
                walk.sorted(Comparator.reverseOrder())
                    .forEach(p -> { try { Files.delete(p); } catch (IOException e) {} });
            }
        }
        Files.createDirectories(path);
    }
}
