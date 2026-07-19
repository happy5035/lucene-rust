CARGO ?= cargo
JAVAC_CP = interop/java/lib/lucene-core-9.12.3.jar:interop/java/lib/lucene-analysis-common-9.12.3.jar

.PHONY: build java-classes interop-test log-test log-bench bench compare

build:
	$(CARGO) build --release

java-classes:
	mkdir -p interop/java/classes
	javac -cp "$(JAVAC_CP)" -d interop/java/classes interop/java/*.java

interop-test: build java-classes
	interop/verify-index.sh 2000 200 42

# M2 log-schema interop: Rust logwrite vs JavaLogBench, CheckIndex + query diff
log-test: build java-classes
	interop/verify-log.sh 200000 42
	interop/verify-log.sh 200000 43 --positions
	interop/verify-log.sh 200000 44 --sparse
	interop/verify-log.sh 200000 45 --bigdict

# M4 log-scenario benchmark: same corpus on both sides
LOGDOCS ?= 1000000
LOGSEED ?= 42
LOGTHREADS ?= 1
log-bench: build java-classes
	rm -rf /tmp/bench-log-rust /tmp/bench-log-java
	mkdir -p /tmp/bench-log-rust /tmp/bench-log-java
	@echo "== Rust logbench ($(LOGDOCS) docs, threads=$(LOGTHREADS))"
	$(CARGO) run -q --release -p rustlucene-core --bin rustlucene-cli -- \
		logbench /tmp/bench-log-rust $(LOGDOCS) $(LOGSEED) $(LOGTHREADS)
	@echo "== Java JavaLogBench"
	java -cp "interop/java/classes:$(JAVAC_CP)" JavaLogBench \
		/tmp/bench-log-java $(LOGDOCS) $(LOGTHREADS) $(LOGSEED)

# Same corpus on both sides; override with e.g. `make bench DOCS=500000 THREADS=1`
DOCS ?= 200000
BYTES ?= 200
SEED ?= 42
THREADS ?= 1
bench: build java-classes
	rm -rf /tmp/bench-rust /tmp/bench-java
	mkdir -p /tmp/bench-rust /tmp/bench-java
	@echo "== Rust ($(DOCS) docs x $(BYTES)B, threads=$(THREADS))"
	$(CARGO) run -q --release -p rustlucene-core --bin rustlucene-cli -- \
		bench /tmp/bench-rust $(DOCS) $(BYTES) $(SEED)
	@echo "== Java"
	java -cp "interop/java/classes:$(JAVAC_CP)" JavaLuceneBench \
		/tmp/bench-java $(DOCS) $(BYTES) $(THREADS) $(SEED)

# Side-by-side corpus comparison: Rust vs Java writer on the same files —
# time/CPU/RSS/index size, CheckIndex on both, then a random 10% term diff.
# Override with e.g. `make compare INPUT=/var/log NDOCS=500000 POSITIONS=--positions`
INPUT ?= /tmp/logcorpus
NDOCS ?=
POSITIONS ?=
compare:
	interop/compare-index.sh $(INPUT) "$(NDOCS)" $(POSITIONS)
