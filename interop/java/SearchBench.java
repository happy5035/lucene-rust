import java.io.IOException;
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.*;
import org.apache.lucene.document.Document;
import org.apache.lucene.index.*;
import org.apache.lucene.search.*;
import org.apache.lucene.store.*;
import org.apache.lucene.util.BytesRef;

/**
 * Search benchmark for Lucene indexes — validates that Rust-built and
 * Java-built indexes deliver identical search performance.
 *
 * Single-index mode:
 *   SearchBench <indexDir> <field> [--warmup N] [--iter N] [--tasks N] [--seed S]
 *
 * Comparison mode (separate JVM per index, identical query set):
 *   1. SearchBench <indexDirA> <field> --dump-queries queries.txt [--tasks N] [--seed S]
 *   2. SearchBench <indexDirA> <field> --load-queries queries.txt --warmup N --iter N > results_a.txt
 *   3. SearchBench <indexDirB> <field> --load-queries queries.txt --warmup N --iter N > results_b.txt
 *   4. diff results_a.txt results_b.txt  (or scripted comparison)
 *
 * Query types (all ConstantScoreQuery, matching Rust's no-scoring profile):
 *   term  — single-term lookup
 *   and   — 2–3 terms, BooleanQuery MUST
 *   or    — 2–3 terms, BooleanQuery SHOULD (minShouldMatch=1)
 *   iterm — (--load-queries only) same TermQuery as "term" but wrapped in
 *           ForceIterQuery (hides the Weight.count docFreq shortcut from
 *           TotalHitCountCollector) and run on a cache-free IndexSearcher
 *           over every TERM line (no sampling), so it measures raw postings
 *           iteration even when the main searcher has the LRUQueryCache
 *           enabled.
 *
 * Flags:
 *   --no-cache — disable the query cache on the main searcher
 *                (setQueryCache(null) + a never-cache policy; default
 *                behaviour is unchanged).
 *
 * Query file format (--dump-queries output, --load-queries input):
 *   TERM\t<bucket>\t<term>\t<docFreq>
 *   AND\t<bucket>\t<term1>\t<term2>   (tasks pairs per bucket)
 *   OR\t<bucket>\t<term1>\t<term2>    (tasks pairs per bucket)
 * --load-queries replays AND/OR lines verbatim (MUST+MUST / SHOULD+SHOULD
 * msm=1, both wrapped in ConstantScoreQuery) when present, and only falls
 * back to self-sampling when the file has none — so Java and the Rust
 * searchbench run the exact same Boolean query set.
 *
 * Terms are sampled from the index dictionary and bucketed by docFreq:
 *   low   — docFreq ≤ 10
 *   med   — 10 < docFreq ≤ 1% maxDoc
 *   high  — docFreq > 1% maxDoc
 *
 * Output (tab-separated, machine-readable):
 *   BENCH query_type freq_bucket qps p50_us p90_us p99_us
 */
public class SearchBench {
    /** 9.12's QueryCachingPolicy has no NEVER_CACHE constant (added in 10.x); inline equivalent. */
    static final QueryCachingPolicy NEVER_CACHE = new QueryCachingPolicy() {
        @Override public void onUse(Query query) {}
        @Override public boolean shouldCache(Query query) { return false; }
    };

    static final class Stats {
        final double qps;
        final double p50us, p90us, p99us;
        Stats(long[] latenciesNs) {
            Arrays.sort(latenciesNs);
            int n = latenciesNs.length;
            double medianNs = n % 2 == 0
                    ? (latenciesNs[n/2 - 1] + latenciesNs[n/2]) / 2.0
                    : latenciesNs[n/2];
            qps = 1_000_000_000.0 / medianNs;
            p50us = percentile(latenciesNs, 50) / 1000.0;
            p90us = percentile(latenciesNs, 90) / 1000.0;
            p99us = percentile(latenciesNs, 99) / 1000.0;
        }
        static long percentile(long[] sorted, double pct) {
            int idx = (int) Math.ceil(pct / 100.0 * sorted.length) - 1;
            return sorted[Math.max(0, Math.min(idx, sorted.length - 1))];
        }
    }

    enum FreqBucket { LOW, MED, HIGH }

    static String bucketLabel(FreqBucket b) {
        switch (b) { case LOW: return "low"; case MED: return "med"; case HIGH: return "high"; }
        return "?";
    }

    static FreqBucket classify(long docFreq, int maxDoc) {
        if (docFreq <= 10) return FreqBucket.LOW;
        if (docFreq <= maxDoc / 100) return FreqBucket.MED;
        return FreqBucket.HIGH;
    }

    static final class TermStats {
        final BytesRef term;
        final long docFreq;
        TermStats(BytesRef t, long df) { this.term = BytesRef.deepCopyOf(t); this.docFreq = df; }
    }

    static Map<FreqBucket, List<TermStats>> collectTerms(
            IndexReader reader, String field, int maxPerBucket, Random rng) throws Exception {
        Map<FreqBucket, List<TermStats>> buckets = new EnumMap<>(FreqBucket.class);
        for (FreqBucket b : FreqBucket.values()) buckets.put(b, new ArrayList<>());

        Terms terms = MultiTerms.getTerms(reader, field);
        if (terms == null) return buckets;

        int maxDoc = reader.maxDoc();
        TermsEnum te = terms.iterator();
        BytesRef term;
        while ((term = te.next()) != null) {
            long df = te.docFreq();
            buckets.get(classify(df, maxDoc)).add(new TermStats(term, df));
        }

        for (FreqBucket b : FreqBucket.values()) {
            List<TermStats> list = buckets.get(b);
            if (list.size() > maxPerBucket) {
                Collections.shuffle(list, rng);
                list.subList(maxPerBucket, list.size()).clear();
            }
            list.sort((a, b2) -> Long.compare(b2.docFreq, a.docFreq));
        }
        return buckets;
    }

    interface QueryGenerator {
        String name();
        Query make(List<TermStats> sample, String field, Random rng);
    }

    static final QueryGenerator TERM_QUERY = new QueryGenerator() {
        public String name() { return "term"; }
        public Query make(List<TermStats> sample, String field, Random rng) {
            TermStats ts = sample.get(rng.nextInt(sample.size()));
            return new ConstantScoreQuery(new TermQuery(new Term(field, ts.term)));
        }
    };

    static final QueryGenerator AND_QUERY = new QueryGenerator() {
        public String name() { return "and"; }
        public Query make(List<TermStats> sample, String field, Random rng) {
            BooleanQuery.Builder b = new BooleanQuery.Builder();
            int n = Math.min(3, sample.size());
            for (int i = 0; i < n; i++) {
                TermStats ts = sample.get(rng.nextInt(sample.size()));
                b.add(new ConstantScoreQuery(new TermQuery(new Term(field, ts.term))),
                      BooleanClause.Occur.MUST);
            }
            return new ConstantScoreQuery(b.build());
        }
    };

    static final QueryGenerator OR_QUERY = new QueryGenerator() {
        public String name() { return "or"; }
        public Query make(List<TermStats> sample, String field, Random rng) {
            BooleanQuery.Builder b = new BooleanQuery.Builder();
            int n = Math.min(3, sample.size());
            for (int i = 0; i < n; i++) {
                TermStats ts = sample.get(rng.nextInt(sample.size()));
                b.add(new ConstantScoreQuery(new TermQuery(new Term(field, ts.term))),
                      BooleanClause.Occur.SHOULD);
            }
            b.setMinimumNumberShouldMatch(1);
            return new ConstantScoreQuery(b.build());
        }
    };

    static final QueryGenerator[] QUERY_TYPES = { TERM_QUERY, AND_QUERY, OR_QUERY };

    /**
     * Wraps a query to force full postings iteration by hiding the
     * {@code Weight.count} shortcut: {@code TotalHitCountCollector} consults
     * {@code weight.count(context)} per leaf and skips iteration when it
     * returns a value, and for an undeleted segment {@code TermQuery}'s weight
     * answers O(1) from the dictionary (TermQuery.java:232-244) — which
     * {@code ConstantScoreQuery} transparently delegates to. A single-MUST
     * {@code BooleanQuery} does NOT help: {@code BooleanWeight.reqCount}
     * returns the only clause's docFreq. Overriding {@code count} to return
     * -1 makes the collector path iterate every hit (DefaultBulkScorer
     * scoreAll), which is exactly what the Rust searchbench iterm mode does.
     */
    static final class ForceIterQuery extends Query {
        private final Query inner;
        ForceIterQuery(Query inner) { this.inner = inner; }

        @Override
        public Query rewrite(IndexReader reader) throws IOException {
            Query rewritten = inner.rewrite(reader);
            if (rewritten != inner) {
                return new ForceIterQuery(rewritten);
            }
            return super.rewrite(reader);
        }

        @Override
        public Weight createWeight(IndexSearcher searcher, ScoreMode scoreMode, float boost)
                throws IOException {
            final Weight in = inner.createWeight(searcher, scoreMode, boost);
            return new FilterWeight(in) {
                @Override
                public int count(LeafReaderContext context) {
                    return -1; // force the collector path to iterate
                }
            };
        }

        @Override
        public void visit(QueryVisitor visitor) {
            inner.visit(visitor.getSubVisitor(BooleanClause.Occur.MUST, this));
        }

        @Override
        public String toString(String field) {
            return "ForceIter(" + inner.toString(field) + ")";
        }

        @Override
        public boolean equals(Object o) {
            return o instanceof ForceIterQuery && inner.equals(((ForceIterQuery) o).inner);
        }

        @Override
        public int hashCode() {
            return 31 * ForceIterQuery.class.hashCode() + inner.hashCode();
        }
    }

    // --- query serialisation for cross-process comparison ------------------

    static String serialiseQuery(QueryGenerator gen, FreqBucket bucket, int idx) {
        return gen.name() + "\t" + bucketLabel(bucket) + "\t" + idx;
    }

    /** Build a flat list of queries from the term dictionary. */
    static List<Query> buildQueries(
            IndexReader reader, String field, int tasksPer, Random rng,
            List<String> labelsOut) throws Exception {
        Map<FreqBucket, List<TermStats>> buckets = collectTerms(reader, field, tasksPer, rng);
        List<Query> queries = new ArrayList<>();
        for (FreqBucket bucket : FreqBucket.values()) {
            List<TermStats> sample = buckets.get(bucket);
            if (sample.isEmpty()) continue;
            for (QueryGenerator gen : QUERY_TYPES) {
                for (int i = 0; i < tasksPer; i++) {
                    queries.add(gen.make(sample, field, rng));
                    labelsOut.add(gen.name() + "\t" + bucketLabel(bucket));
                }
            }
        }
        return queries;
    }

    // --- benchmark ---------------------------------------------------------

    @SuppressWarnings("deprecation")
    static long runOnce(IndexSearcher searcher, Query query) throws Exception {
        long t0 = System.nanoTime();
        TotalHitCountCollector collector = new TotalHitCountCollector();
        searcher.search(query, collector);
        return System.nanoTime() - t0;
    }

    static Stats measure(IndexSearcher searcher, Query query, int warmup, int iter) throws Exception {
        for (int i = 0; i < warmup; i++) runOnce(searcher, query);
        long[] lats = new long[iter];
        for (int i = 0; i < iter; i++) lats[i] = runOnce(searcher, query);
        return new Stats(lats);
    }

    // --- main ---------------------------------------------------------------

    public static void main(String[] args) throws Exception {
        int warmup = 10, iterations = 20, tasks = 100;
        long seed = 42;
        String dumpQueriesFile = null;
        String loadQueriesFile = null;
        boolean noCache = false;
        List<String> pos = new ArrayList<>();

        for (int i = 0; i < args.length; i++) {
            switch (args[i]) {
                case "--warmup":    warmup = Integer.parseInt(args[++i]); break;
                case "--iter":      iterations = Integer.parseInt(args[++i]); break;
                case "--tasks":     tasks = Integer.parseInt(args[++i]); break;
                case "--seed":      seed = Long.parseLong(args[++i]); break;
                case "--dump-queries": dumpQueriesFile = args[++i]; break;
                case "--load-queries": loadQueriesFile = args[++i]; break;
                case "--no-cache":  noCache = true; break;
                default: pos.add(args[i]);
            }
        }
        if (pos.size() < 2) {
            System.err.println("usage: SearchBench <indexDir> <field> [--tasks N] [--warmup N] [--iter N] [--seed S] [--no-cache] [--dump-queries|--load-queries FILE]");
            System.exit(2);
        }

        Path indexDir = Paths.get(pos.get(0));
        String field = pos.get(1);

        try (DirectoryReader reader = DirectoryReader.open(FSDirectory.open(indexDir))) {
            IndexSearcher searcher = new IndexSearcher(reader);
            if (noCache) {
                searcher.setQueryCache(null);
                searcher.setQueryCachingPolicy(NEVER_CACHE);
            }
            Random rng = new Random(seed);

            if (dumpQueriesFile != null) {
                // --- dump query definitions for cross-process comparison ---
                Map<FreqBucket, List<TermStats>> buckets = collectTerms(reader, field, tasks, rng);
                try (java.io.PrintWriter pw = new java.io.PrintWriter(dumpQueriesFile)) {
                    for (FreqBucket bucket : FreqBucket.values()) {
                        for (TermStats ts : buckets.get(bucket)) {
                            pw.printf(Locale.ROOT, "TERM\t%s\t%s\t%d%n",
                                    bucketLabel(bucket), ts.term.utf8ToString(), ts.docFreq);
                        }
                    }
                    // AND/OR term pairs, mirroring the --load-queries generation
                    // logic (2 terms per query, `tasks` queries per bucket).
                    for (FreqBucket bucket : FreqBucket.values()) {
                        List<TermStats> sample = buckets.get(bucket);
                        if (sample.size() < 2) continue;
                        for (int i = 0; i < tasks; i++) {
                            String t1 = sample.get(rng.nextInt(sample.size())).term.utf8ToString();
                            String t2 = sample.get(rng.nextInt(sample.size())).term.utf8ToString();
                            pw.printf(Locale.ROOT, "AND\t%s\t%s\t%s%n", bucketLabel(bucket), t1, t2);
                        }
                        for (int i = 0; i < tasks; i++) {
                            String t1 = sample.get(rng.nextInt(sample.size())).term.utf8ToString();
                            String t2 = sample.get(rng.nextInt(sample.size())).term.utf8ToString();
                            pw.printf(Locale.ROOT, "OR\t%s\t%s\t%s%n", bucketLabel(bucket), t1, t2);
                        }
                    }
                }
                System.out.println("DUMPED terms to " + dumpQueriesFile);
                return;
            }

            if (loadQueriesFile != null) {
                // --- load queries from a previous --dump-queries run ---
                Map<FreqBucket, List<String>> loadedTerms = new EnumMap<>(FreqBucket.class);
                for (FreqBucket b : FreqBucket.values()) loadedTerms.put(b, new ArrayList<>());
                // AND/OR lines: {op, bucketLabel, term1, term2}, kept in file order
                List<String[]> loadedBool = new ArrayList<>();
                for (String line : Files.readAllLines(Paths.get(loadQueriesFile))) {
                    String[] parts = line.split("\t");
                    if (parts[0].equals("TERM") && parts.length >= 3) {
                        FreqBucket bucket = FreqBucket.valueOf(parts[1].toUpperCase());
                        loadedTerms.get(bucket).add(parts[2]);
                    } else if ((parts[0].equals("AND") || parts[0].equals("OR")) && parts.length >= 4) {
                        loadedBool.add(new String[]{
                                parts[0].toLowerCase(Locale.ROOT), parts[1], parts[2], parts[3]});
                    }
                }
                // Build queries using the loaded terms
                List<Query> queries = new ArrayList<>();
                List<String> labels = new ArrayList<>();
                List<String> details = new ArrayList<>();
                for (FreqBucket bucket : FreqBucket.values()) {
                    List<String> terms = loadedTerms.get(bucket);
                    if (terms.isEmpty()) continue;
                    for (int i = 0; i < tasks && i < terms.size(); i++) {
                        String t = terms.get(rng.nextInt(terms.size()));
                        queries.add(new ConstantScoreQuery(new TermQuery(new Term(field, t))));
                        labels.add("term\t" + bucketLabel(bucket));
                        details.add("term=" + t + " bucket=term\t" + bucketLabel(bucket));
                    }
                    if (!loadedBool.isEmpty()) continue;  // AND/OR replayed from file below
                    // AND queries: pick 2 terms from this bucket
                    for (int i = 0; i < tasks && terms.size() >= 2; i++) {
                        String t1 = terms.get(rng.nextInt(terms.size()));
                        String t2 = terms.get(rng.nextInt(terms.size()));
                        BooleanQuery.Builder b = new BooleanQuery.Builder();
                        b.add(new ConstantScoreQuery(new TermQuery(new Term(field, t1))), BooleanClause.Occur.MUST);
                        b.add(new ConstantScoreQuery(new TermQuery(new Term(field, t2))), BooleanClause.Occur.MUST);
                        queries.add(new ConstantScoreQuery(b.build()));
                        labels.add("and\t" + bucketLabel(bucket));
                        details.add("and t1=" + t1 + " t2=" + t2 + " bucket=and\t" + bucketLabel(bucket));
                    }
                    // OR queries
                    for (int i = 0; i < tasks && terms.size() >= 2; i++) {
                        String t1 = terms.get(rng.nextInt(terms.size()));
                        String t2 = terms.get(rng.nextInt(terms.size()));
                        BooleanQuery.Builder b = new BooleanQuery.Builder();
                        b.add(new ConstantScoreQuery(new TermQuery(new Term(field, t1))), BooleanClause.Occur.SHOULD);
                        b.add(new ConstantScoreQuery(new TermQuery(new Term(field, t2))), BooleanClause.Occur.SHOULD);
                        b.setMinimumNumberShouldMatch(1);
                        queries.add(new ConstantScoreQuery(b.build()));
                        labels.add("or\t" + bucketLabel(bucket));
                        details.add("or t1=" + t1 + " t2=" + t2 + " bucket=or\t" + bucketLabel(bucket));
                    }
                }
                // AND/OR lines from the file take precedence over self-sampling:
                // build the queries exactly as recorded, in file order.
                for (String[] p : loadedBool) {
                    String op = p[0], bl = p[1], t1 = p[2], t2 = p[3];
                    BooleanQuery.Builder b = new BooleanQuery.Builder();
                    BooleanClause.Occur occur = op.equals("and")
                            ? BooleanClause.Occur.MUST : BooleanClause.Occur.SHOULD;
                    b.add(new ConstantScoreQuery(new TermQuery(new Term(field, t1))), occur);
                    b.add(new ConstantScoreQuery(new TermQuery(new Term(field, t2))), occur);
                    if (op.equals("or")) b.setMinimumNumberShouldMatch(1);
                    queries.add(new ConstantScoreQuery(b.build()));
                    labels.add(op + "\t" + bl);
                    details.add(op + " t1=" + t1 + " t2=" + t2 + " bucket=" + op + "\t" + bl);
                }
                // ITERM queries: every TERM line (bucket enum order, file order
                // within a bucket — matching the Rust searchbench iterm block),
                // no sampling. They run on a dedicated cache-free searcher so
                // the forced-iteration baseline stays honest even when the main
                // searcher's LRUQueryCache is enabled.
                IndexSearcher iterSearcher = new IndexSearcher(reader);
                iterSearcher.setQueryCache(null);
                iterSearcher.setQueryCachingPolicy(NEVER_CACHE);
                for (FreqBucket bucket : FreqBucket.values()) {
                    for (String t : loadedTerms.get(bucket)) {
                        queries.add(new ForceIterQuery(
                                new ConstantScoreQuery(new TermQuery(new Term(field, t)))));
                        labels.add("iterm\t" + bucketLabel(bucket));
                        details.add("iterm=" + t + " bucket=iterm\t" + bucketLabel(bucket));
                    }
                }
                runAndPrint(searcher, iterSearcher, queries, labels, details, warmup, iterations);
                return;
            }

            // --- default: build queries from index and benchmark ---
            List<String> labels = new ArrayList<>();
            List<Query> queries = buildQueries(reader, field, tasks, rng, labels);
            runAndPrint(searcher, queries, labels, warmup, iterations);
        }
    }

    static void runAndPrint(IndexSearcher searcher, List<Query> queries, List<String> labels,
                            int warmup, int iterations) throws Exception {
        runAndPrint(searcher, queries, labels, null, warmup, iterations);
    }

    static void runAndPrint(IndexSearcher searcher, List<Query> queries, List<String> labels,
                            List<String> details, int warmup, int iterations) throws Exception {
        runAndPrint(searcher, null, queries, labels, details, warmup, iterations);
    }

    static void runAndPrint(IndexSearcher searcher, IndexSearcher iterSearcher,
                            List<Query> queries, List<String> labels,
                            List<String> details, int warmup, int iterations) throws Exception {
        // Per-query searcher selection: labels starting with "iterm" run on the
        // cache-free iterSearcher (when provided); everything else uses the
        // main searcher.
        IndexSearcher[] searchers = new IndexSearcher[queries.size()];
        for (int i = 0; i < queries.size(); i++) {
            searchers[i] = iterSearcher != null && labels.get(i).startsWith("iterm")
                    ? iterSearcher : searcher;
        }

        // Global warmup: run all queries once to prime JIT and page cache
        for (int i = 0; i < queries.size(); i++) {
            runOnce(searchers[i], queries.get(i));
        }

        // Per-query measurement
        Map<String, List<Double>> aggQps = new LinkedHashMap<>();
        Map<String, List<Double>> aggP50 = new LinkedHashMap<>();
        Map<String, List<Double>> aggP90 = new LinkedHashMap<>();
        Map<String, List<Double>> aggP99 = new LinkedHashMap<>();

        for (int i = 0; i < queries.size(); i++) {
            Stats s = measure(searchers[i], queries.get(i), warmup, iterations);
            String group = labels.get(i);
            aggQps.computeIfAbsent(group, k -> new ArrayList<>()).add(s.qps);
            aggP50.computeIfAbsent(group, k -> new ArrayList<>()).add(s.p50us);
            aggP90.computeIfAbsent(group, k -> new ArrayList<>()).add(s.p90us);
            aggP99.computeIfAbsent(group, k -> new ArrayList<>()).add(s.p99us);
            if (details != null) {
                // Per-query hit count for correctness diffing against the Rust
                // searchbench (which prints the same lines to stderr).
                TotalHitCountCollector c = new TotalHitCountCollector();
                searchers[i].search(queries.get(i), c);
                System.err.printf(Locale.ROOT, "%s\t%d%n", details.get(i), c.getTotalHits());
            }
        }

        // Print aggregated results
        System.out.printf(Locale.ROOT, "%s\t%s\t%s\t%s\t%s\t%s%n",
                "query_type", "freq", "qps", "p50_us", "p90_us", "p99_us");
        for (String group : aggQps.keySet()) {
            List<Double> qpsList = aggQps.get(group);
            double avgQps = qpsList.stream().mapToDouble(Double::doubleValue).average().orElse(0);
            double avgP50 = aggP50.get(group).stream().mapToDouble(Double::doubleValue).average().orElse(0);
            double avgP90 = aggP90.get(group).stream().mapToDouble(Double::doubleValue).average().orElse(0);
            double avgP99 = aggP99.get(group).stream().mapToDouble(Double::doubleValue).average().orElse(0);
            String[] parts = group.split("\t");
            System.out.printf(Locale.ROOT, "%s\t%s\t%.1f\t%.1f\t%.1f\t%.1f%n",
                    parts[0], parts[1], avgQps, avgP50, avgP90, avgP99);
        }
    }
}
