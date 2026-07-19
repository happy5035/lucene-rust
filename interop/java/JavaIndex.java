import java.io.BufferedReader;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.util.*;
import org.apache.lucene.analysis.core.WhitespaceAnalyzer;
import org.apache.lucene.document.*;
import org.apache.lucene.index.*;
import org.apache.lucene.store.FSDirectory;

/**
 * Java twin of `rustlucene-cli index <input> <dir> [--positions] [--docs N]`
 * (crates/core/src/bin/rustlucene-cli.rs). Same corpus walk (recursive,
 * sorted), same document shape, same --docs cycling semantics, so the two
 * indexes are directly comparable (term postings must come out identical).
 *
 * Documents: `message` (indexed + stored, WhitespaceAnalyzer — the Rust side
 * tokenizes with split_ascii_whitespace, equivalent on ASCII input),
 * `source` (stored-only relative path), `line` (stored-only line number).
 * No-scoring profile: DOCS_AND_FREQS (+positions optional), omitNorms.
 *
 * Config: single flush at commit (large RAM buffer, no doc-count flush) and
 * no compound file, matching the Rust writer's single-segment output.
 *
 * Output format matches the Rust CLI so scripts can parse both:
 *   INDEXED files=.. docs=.. skipped_lines=.. elapsed_ms=.. docs_per_sec=.. positions=..
 *
 * Note on malformed input: the Rust side skips non-UTF-8 lines; Java's UTF-8
 * decoder aborts the file on malformed input instead (our corpora are clean).
 */
public class JavaIndex {
    public static void main(String[] args) throws Exception {
        if (args.length < 2) {
            System.err.println("usage: JavaIndex <inputFileOrDir> <indexDir> [--positions] [--docs N]");
            System.exit(2);
        }
        Path input = Paths.get(args[0]);
        Path indexDir = Paths.get(args[1]);
        boolean positions = false;
        Long targetDocs = null;
        for (int i = 2; i < args.length; i++) {
            switch (args[i]) {
                case "--positions":
                    positions = true;
                    break;
                case "--docs":
                    targetDocs = Long.parseLong(args[++i]);
                    break;
                default:
                    System.err.println("unknown argument: " + args[i]);
                    System.exit(2);
            }
        }

        FieldType msgType = new FieldType();
        msgType.setTokenized(true);
        msgType.setStored(true);
        msgType.setIndexOptions(positions
                ? IndexOptions.DOCS_AND_FREQS_AND_POSITIONS
                : IndexOptions.DOCS_AND_FREQS);
        msgType.setOmitNorms(true);
        msgType.freeze();

        List<Path> files = new ArrayList<>();
        if (Files.isRegularFile(input)) {
            files.add(input);
        } else if (Files.isDirectory(input)) {
            try (var walk = Files.walk(input)) {
                walk.filter(Files::isRegularFile).forEach(files::add);
            }
        }
        files.sort(Comparator.naturalOrder());
        if (files.isEmpty()) {
            System.err.println("no input files under " + input);
            System.exit(2);
        }

        IndexWriterConfig iwc = new IndexWriterConfig(new WhitespaceAnalyzer());
        iwc.setRAMBufferSizeMB(1024.0);
        iwc.setMaxBufferedDocs(IndexWriterConfig.DISABLE_AUTO_FLUSH);
        iwc.setUseCompoundFile(false);

        long docs = 0, skipped = 0;
        long target = targetDocs == null ? Long.MAX_VALUE : targetDocs;
        try (FSDirectory dir = FSDirectory.open(indexDir);
             IndexWriter w = new IndexWriter(dir, iwc)) {
            long t0 = System.nanoTime();
            boolean isDir = Files.isDirectory(input);
            outer:
            while (true) {
                long passStart = docs;
                for (Path f : files) {
                    String source = isDir
                            ? input.relativize(f).toString()
                            : f.getFileName().toString();
                    try (BufferedReader br = Files.newBufferedReader(f, StandardCharsets.UTF_8)) {
                        long lineno = 0;
                        String line;
                        while ((line = br.readLine()) != null) {
                            if (docs >= target) {
                                break outer;
                            }
                            lineno++;
                            String text = line.trim();
                            if (text.isEmpty()) {
                                continue;
                            }
                            Document d = new Document();
                            d.add(new Field("message", text, msgType));
                            d.add(new StoredField("source", source));
                            d.add(new StoredField("line", Long.toString(lineno)));
                            w.addDocument(d);
                            docs++;
                        }
                    }
                }
                if (targetDocs == null || docs >= target) {
                    break;
                }
                if (docs == passStart) {
                    System.err.println("input under " + input
                            + " has no indexable lines; cannot reach --docs " + target);
                    System.exit(2);
                }
            }
            w.commit();
            long ms = Math.max(1, (System.nanoTime() - t0) / 1_000_000);
            System.out.printf(Locale.ROOT,
                    "INDEXED files=%d docs=%d skipped_lines=%d elapsed_ms=%d docs_per_sec=%.0f positions=%b%n",
                    files.size(), docs, skipped, ms, docs * 1000.0 / ms, positions);
        }
    }
}
