import java.io.IOException;

/**
 * Java wrapper for the RustLucene writer over JNI. Mirrors the IndexWriter
 * subset: open with a path + schema spec, add documents field by field,
 * flush/commit, close.
 *
 * Schema spec example:
 *   "timestamp:longpoint+numericdv+stored,level:keyword+sorteddv,
 *    trace_id:keyword,message:text+positions,latency_ms:numericdv"
 *
 * Threading: the native handle serializes calls internally; for parallel
 * ingestion use one RustIndexWriter per thread and merge via commit (the
 * sharded-segments model), like the Rust CLI bench does.
 */
public class RustIndexWriter implements AutoCloseable {
    static {
        System.loadLibrary("rustlucene_jni");
    }

    private long handle;

    public RustIndexWriter(String path, String schemaSpec) throws IOException {
        this.handle = nativeCreate(path, schemaSpec);
        if (this.handle == 0) {
            throw new IOException("native writer creation failed: " + path);
        }
    }

    public void beginDocument() { nativeBeginDocument(handle); }
    public void addText(String field, String value) { nativeAddText(handle, field, value); }
    public void addKeyword(String field, String value) { nativeAddKeyword(handle, field, value); }
    public void addLong(String field, long value) { nativeAddLong(handle, field, value); }
    public void addInt(String field, int value) { nativeAddInt(handle, field, value); }
    public void endDocument() { nativeEndDocument(handle); }

    /**
     * Adds a batch of raw JSON documents (one flat JSON object per element,
     * UTF-8 bytes). Parsing, schema binding, type coercion and the
     * unknown-field policy (schema-spec {@code $policy=...}) all run inside
     * Rust; the whole batch crosses JNI once. A malformed line or a failing
     * document never aborts the batch.
     *
     * @return packed result: {@code (okCount << 32) | (failedCount & 0xffffffffL)}
     */
    public long addJsonBatch(byte[][] docs) { return nativeAddJsonBatch(handle, docs); }

    public void flush() { nativeFlush(handle); }
    public void commit() { nativeCommit(handle); }

    @Override
    public void close() {
        if (handle != 0) {
            nativeClose(handle);
            handle = 0;
        }
    }

    private static native long nativeCreate(String path, String schemaSpec);
    private static native long nativeBeginDocument(long handle);
    private static native long nativeAddText(long handle, String field, String value);
    private static native long nativeAddKeyword(long handle, String field, String value);
    private static native long nativeAddLong(long handle, String field, long value);
    private static native long nativeAddInt(long handle, String field, int value);
    private static native long nativeEndDocument(long handle);
    private static native long nativeAddJsonBatch(long handle, byte[][] docs);
    private static native long nativeFlush(long handle);
    private static native long nativeCommit(long handle);
    private static native long nativeClose(long handle);
}
