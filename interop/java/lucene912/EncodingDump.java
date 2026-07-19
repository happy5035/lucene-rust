package org.apache.lucene.codecs.lucene912;

import java.io.ByteArrayOutputStream;
import java.io.PrintStream;
import org.apache.lucene.store.DataOutput;
import org.apache.lucene.store.OutputStreamDataOutput;

/**
 * Dumps reference encodings from the real Lucene 9.12.3 ForUtil / PForUtil /
 * ForDeltaUtil (package-private classes, accessed from the same package).
 * Output lines: <name>=<hex>. The Rust side compares its encoders byte-for-byte.
 */
public class EncodingDump {
    public static void main(String[] args) throws Exception {
        PrintStream out = System.out;

        // vector sets (deterministic, hardcoded — mirrored in the Rust test)
        long[] mixed = new long[128];
        for (int i = 0; i < 128; i++) mixed[i] = (i * 37L) % 50 + 1; // 1..50

        long[] ones = new long[128];
        java.util.Arrays.fill(ones, 1);

        long[] freqs = new long[128];
        for (int i = 0; i < 128; i++) freqs[i] = i % 5 + 1;

        long[] freqsExc = freqs.clone();
        freqsExc[3] = 3000; freqsExc[77] = 65535; freqsExc[100] = 999;

        long[] freqsManyExc = freqs.clone();
        for (int i = 0; i < 10; i++) freqsManyExc[i * 3] = 5000 + i;

        ForUtil forUtil = new ForUtil();
        PForUtil pfor = new PForUtil(forUtil);
        ForDeltaUtil forDelta = new ForDeltaUtil();

        // ForUtil at several bit widths
        for (int bpv : new int[] {1, 3, 9, 16}) {
            long[] v = new long[128];
            for (int i = 0; i < 128; i++) v[i] = (i * 37L) % (1L << bpv);
            ByteArrayOutputStream buf = new ByteArrayOutputStream();
            forUtil.encode(v, bpv, new OutputStreamDataOutput(buf));
            out.println("for_bpv" + bpv + "=" + hex(buf.toByteArray()));
        }

        ByteArrayOutputStream buf = new ByteArrayOutputStream();
        forDelta.encodeDeltas(ones, new OutputStreamDataOutput(buf));
        out.println("delta_ones=" + hex(buf.toByteArray()));

        buf = new ByteArrayOutputStream();
        forDelta.encodeDeltas(mixed, new OutputStreamDataOutput(buf));
        out.println("delta_mixed=" + hex(buf.toByteArray()));

        buf = new ByteArrayOutputStream();
        pfor.encode(freqs, new OutputStreamDataOutput(buf));
        out.println("pfor_freqs=" + hex(buf.toByteArray()));

        buf = new ByteArrayOutputStream();
        pfor.encode(freqsExc, new OutputStreamDataOutput(buf));
        out.println("pfor_exc=" + hex(buf.toByteArray()));

        buf = new ByteArrayOutputStream();
        pfor.encode(freqsManyExc, new OutputStreamDataOutput(buf));
        out.println("pfor_manyexc=" + hex(buf.toByteArray()));
    }

    static String hex(byte[] b) {
        StringBuilder sb = new StringBuilder(b.length * 2);
        for (byte x : b) sb.append(Character.forDigit((x >> 4) & 0xF, 16)).append(Character.forDigit(x & 0xF, 16));
        return sb.toString();
    }
}
