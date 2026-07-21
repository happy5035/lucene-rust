import java.io.BufferedReader;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.util.*;
import org.apache.lucene.analysis.core.WhitespaceAnalyzer;
import org.apache.lucene.document.*;
import org.apache.lucene.index.*;
import org.apache.lucene.store.FSDirectory;
import org.apache.lucene.util.BytesRef;

/**
 * Stock-Lucene baseline for the JSON batch path: reads the JSONL corpus
 * produced by `rustlucene-cli jsongen` and indexes it with the same document
 * shape the Rust schema spec declares:
 *   timestamp  = LongPoint + NumericDocValues + StoredField
 *   level      = StringField + SortedDocValues
 *   trace_id   = StringField
 *   message    = TextField (WhitespaceAnalyzer, DOCS_AND_FREQS, omitNorms, stored)
 *   latency_ms = IntPoint + NumericDocValues + StoredField
 * Unknown fields (noise_payload) are dropped, matching $policy=strict.
 *
 * JSON is parsed with a small hand-written flat-object parser (string/number/
 * bool/null values only — exactly the jsongen output shape); the interop
 * classpath deliberately carries no JSON library.
 *
 * Single flush at commit, no compound file — same config as JavaIndex.
 *
 * Usage: JavaJsonIndex <jsonlFile> <indexDir> [--docs N]
 * Output: INDEXED docs=.. skipped=.. elapsed_ms=.. docs_per_sec=..
 */
public class JavaJsonIndex {

    /** Minimal flat-JSON-object parser: {"k":value,...} with string/number/bool/null values. */
    static final class FlatJson {
        final String s;
        int p;
        FlatJson(String s) { this.s = s; }

        /** Returns key->value (String, Long, Double, Boolean, or null); null on parse failure. */
        Map<String, Object> parseObject() {
            skipWs();
            if (!expect('{')) return null;
            Map<String, Object> out = new LinkedHashMap<>();
            skipWs();
            if (peek('}')) { p++; return out; }
            while (true) {
                skipWs();
                String key = parseString();
                if (key == null) return null;
                skipWs();
                if (!expect(':')) return null;
                skipWs();
                if (!parseValue(out, key)) return null;
                skipWs();
                if (peek(',')) { p++; continue; }
                if (peek('}')) { p++; return out; }
                return null;
            }
        }

        private boolean parseValue(Map<String, Object> out, String key) {
            char c = p < s.length() ? s.charAt(p) : '\0';
            if (c == '"') {
                String v = parseString();
                if (v == null) return false;
                out.put(key, v);
                return true;
            }
            if (c == 't' && s.startsWith("true", p)) { p += 4; out.put(key, Boolean.TRUE); return true; }
            if (c == 'f' && s.startsWith("false", p)) { p += 5; out.put(key, Boolean.FALSE); return true; }
            if (c == 'n' && s.startsWith("null", p)) { p += 4; out.put(key, null); return true; }
            if (c == '-' || (c >= '0' && c <= '9')) {
                int start = p;
                boolean floating = false;
                while (p < s.length()) {
                    char d = s.charAt(p);
                    if (d == '.' || d == 'e' || d == 'E') floating = true;
                    if (d == '-' || d == '+' || d == '.' || d == 'e' || d == 'E' || (d >= '0' && d <= '9')) p++;
                    else break;
                }
                String num = s.substring(start, p);
                try {
                    out.put(key, floating ? (Object) Double.valueOf(num) : (Object) Long.valueOf(num));
                } catch (NumberFormatException e) {
                    return false;
                }
                return true;
            }
            return false; // nested object/array: not supported (jsongen never emits them)
        }

        private String parseString() {
            if (!expect('"')) return null;
            StringBuilder sb = new StringBuilder();
            while (p < s.length()) {
                char c = s.charAt(p++);
                if (c == '"') return sb.toString();
                if (c == '\\') {
                    if (p >= s.length()) return null;
                    char e = s.charAt(p++);
                    switch (e) {
                        case 'n': sb.append('\n'); break;
                        case 't': sb.append('\t'); break;
                        case 'r': sb.append('\r'); break;
                        case 'u':
                            if (p + 4 > s.length()) return null;
                            sb.append((char) Integer.parseInt(s.substring(p, p + 4), 16));
                            p += 4;
                            break;
                        default: sb.append(e);
                    }
                } else {
                    sb.append(c);
                }
            }
            return null;
        }

        private void skipWs() { while (p < s.length() && Character.isWhitespace(s.charAt(p))) p++; }
        private boolean peek(char c) { return p < s.length() && s.charAt(p) == c; }
        private boolean expect(char c) { if (!peek(c)) return false; p++; return true; }
    }

    public static void main(String[] args) throws Exception {
        if (args.length < 2) {
            System.err.println("usage: JavaJsonIndex <jsonlFile> <indexDir> [--docs N]");
            System.exit(2);
        }
        Path input = Paths.get(args[0]);
        Path indexDir = Paths.get(args[1]);
        long target = Long.MAX_VALUE;
        for (int i = 2; i < args.length; i++) {
            if (args[i].equals("--docs")) {
                target = Long.parseLong(args[++i]);
            } else {
                System.err.println("unknown argument: " + args[i]);
                System.exit(2);
            }
        }

        FieldType msgType = new FieldType();
        msgType.setTokenized(true);
        msgType.setStored(true);
        msgType.setIndexOptions(IndexOptions.DOCS_AND_FREQS);
        msgType.setOmitNorms(true);
        msgType.freeze();

        IndexWriterConfig iwc = new IndexWriterConfig(new WhitespaceAnalyzer());
        iwc.setRAMBufferSizeMB(1024.0);
        iwc.setMaxBufferedDocs(IndexWriterConfig.DISABLE_AUTO_FLUSH);
        iwc.setUseCompoundFile(false);

        long docs = 0, skipped = 0;
        try (FSDirectory dir = FSDirectory.open(indexDir);
             IndexWriter w = new IndexWriter(dir, iwc);
             BufferedReader br = Files.newBufferedReader(input, StandardCharsets.UTF_8)) {
            long t0 = System.nanoTime();
            String line;
            while ((line = br.readLine()) != null && docs < target) {
                if (line.isBlank()) continue;
                Map<String, Object> obj = new FlatJson(line).parseObject();
                if (obj == null) { skipped++; continue; }
                Document d = new Document();
                Object ts = obj.get("timestamp");
                if (ts instanceof Long v) {
                    d.add(new LongPoint("timestamp", v));
                    d.add(new NumericDocValuesField("timestamp", v));
                    d.add(new StoredField("timestamp", v));
                }
                Object level = obj.get("level");
                if (level instanceof String v) {
                    d.add(new StringField("level", v, Field.Store.YES));
                    d.add(new SortedDocValuesField("level", new BytesRef(v)));
                }
                Object traceId = obj.get("trace_id");
                if (traceId instanceof String v) {
                    d.add(new StringField("trace_id", v, Field.Store.YES));
                }
                Object message = obj.get("message");
                if (message instanceof String v) {
                    d.add(new Field("message", v, msgType));
                }
                Object latency = obj.get("latency_ms");
                if (latency instanceof Long v) {
                    d.add(new IntPoint("latency_ms", v.intValue()));
                    d.add(new NumericDocValuesField("latency_ms", v));
                    d.add(new StoredField("latency_ms", v.intValue()));
                }
                w.addDocument(d);
                docs++;
            }
            w.commit();
            long ms = Math.max(1, (System.nanoTime() - t0) / 1_000_000);
            System.out.printf(Locale.ROOT,
                    "INDEXED docs=%d skipped=%d elapsed_ms=%d docs_per_sec=%.0f%n",
                    docs, skipped, ms, docs * 1000.0 / ms);
        }
    }
}
