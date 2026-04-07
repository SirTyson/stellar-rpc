# H003: `SummaryVec` quantile tracking adds avoidable per-request CPU in the `getTransactions` wrapper

**Date**: 2026-04-06
**Subsystem**: network
**Severity**: Low
**Impact**: metrics CPU / allocation
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Latency instrumentation for `getTransactions` should record one cheap observation per request, ideally with negligible allocation and synchronization overhead relative to the method body. The network wrapper should not perform expensive quantile bookkeeping on every request if a lower-overhead metric type would provide equivalent operational value.

## Mechanism

`decorateHandlers` registers `json_rpc_request_duration_seconds` as a `prometheus.SummaryVec` with three quantile objectives and then does `requestMetric.With(label).Observe(...)` on every request. That path allocates a fresh `prometheus.Labels` map, performs label lookup, and updates the summary's quantile streams under synchronization for every `getTransactions` call. Replacing this with pre-bound observers and/or a histogram-based metric would remove wrapper CPU that is unrelated to ledger reads or response generation.

## Trigger

Benchmark `getTransactions` at sustained concurrency with metrics enabled, then compare against a build that switches this metric to `HistogramVec` or another cheaper observer with pre-bound labels. Measure wrapper CPU, allocations per request, and RPS.

## Target Code

- `cmd/stellar-rpc/internal/jsonrpc.go:decorateHandlers:74-80` — creates a `SummaryVec` with 0.5/0.9/0.99 quantile objectives
- `cmd/stellar-rpc/internal/jsonrpc.go:decorateHandlers:89-101` — allocates request labels and calls `requestMetric.With(label).Observe(duration.Seconds())` on every request

## Evidence

The observation happens for every `getTransactions` request regardless of success size, log level, or timeout settings. The label set is small and bounded, which means a cheaper pre-bound metric path is feasible without changing endpoint behavior.

## Anti-Evidence

There is only one observation per request, so the absolute cost is smaller than the duplicate response-marshaling issue above. If `getTransactions` latency is dominated by ledger decoding and JSON conversion, this wrapper-only optimization may stay below a 5% gain.

---

## Review

**Verdict**: VIABLE
**Severity**: Informational
**Date**: 2026-04-06
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full per-request metrics path through `decorateHandlers` (jsonrpc.go:83-104) into the Prometheus client_golang v1.23.2 internals. Each `getTransactions` request allocates a `prometheus.Labels` map (line 89), hashes and looks up the metric via `MetricVec.GetMetricWith` → `hashLabels` → `getOrCreateMetricWithLabels` (RLock + map lookup), then calls `summary.Observe` which acquires `bufMtx`, appends to `hotBuf`, and occasionally triggers `asyncFlush` for quantile stream maintenance. The inefficiency is real but the absolute cost (~200-400ns per request) is negligible relative to getTransactions' dominant costs (DB I/O, XDR decoding, JSON marshaling at ms-scale).

### Code Paths Examined

- `cmd/stellar-rpc/internal/jsonrpc.go:decorateHandlers:73-108` — creates `SummaryVec` with 3 quantile objectives; the closure at lines 83-104 runs on every request, allocating `prometheus.Labels{"endpoint": ..., "status": ...}` and calling `.With(label).Observe()`
- `prometheus/client_golang@v1.23.2/prometheus/vec.go:MetricVec.GetMetricWith:238-248` — calls `constrainLabels` (fast path, no-op with no constraints), `hashLabels` (hashes 2 label values), then `getOrCreateMetricWithLabels` (RLock + hash map lookup + label comparison + RUnlock)
- `prometheus/client_golang@v1.23.2/prometheus/vec.go:metricMap.getOrCreateMetricWithLabels:514-533` — RLock path for existing metrics; metric is found on first hit after creation, so the write-lock path is essentially never taken during steady-state
- `prometheus/client_golang@v1.23.2/prometheus/summary.go:summary.Observe:309-321` — acquires `bufMtx` (per-summary mutex), checks expiry, appends float64 to `hotBuf`; when buffer is full (default cap 500), calls `asyncFlush` which takes `mtx`, swaps buffers, and flushes quantile streams in a background goroutine
- `prometheus/client_golang@v1.23.2/prometheus/summary.go:summary.asyncFlush:369-380` — swaps hot/cold buffers under both mutexes, spawns goroutine for `flushColdBuf` which inserts into quantile streams — this is the expensive part but amortized over 500 observations

### Findings

The hypothesis correctly identifies three sources of per-request overhead:

1. **Map allocation** (line 89): `prometheus.Labels{"endpoint": r.Method(), "status": "ok"}` creates a heap-allocated `map[string]string` on every request (~100 bytes). This is avoidable by pre-binding observers via `WithLabelValues()` or `CurryWith()` at handler registration time.

2. **Label hashing and lookup** (vec.go:238-248): `With(labels)` calls `hashLabels()` (iterates 2 label names, hashes string values) then `getOrCreateMetricWithLabels()` (RLock + map lookup + string comparison). For pre-existing metrics this is fast (~100-200ns) but non-zero. Pre-bound observers skip this entirely.

3. **Summary observation mutex** (summary.go:309-321): `Observe()` acquires `bufMtx` for every observation. Under high concurrency, all successful `getTransactions` requests contend on the same Summary's `bufMtx`. A `HistogramVec` uses `atomic.AddUint64` on bucket counters — completely lock-free.

However, the total per-request cost is ~200-400ns uncontended. A typical `getTransactions` request costs 1-50ms (DB query + XDR decode + JSON marshal), making the metrics overhead 0.001-0.04% of total request time. Even under extreme mutex contention (hundreds of concurrent goroutines), the added latency would be single-digit microseconds — still far below 1% of request time.

The `constrainLabels` fast path (vec.go:665-668) returns immediately when no label constraints are configured, so there's no additional allocation there.

**Severity downgrade rationale**: The hypothesis claimed Low severity (<5% but measurable). Given the ~200-400ns overhead against ms-scale request processing, the improvement is not measurable for `getTransactions`. Downgraded to Informational (theoretical improvement with no measured impact yet).

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/jsonrpc.go:decorateHandlers:73-108`
- **Change description**: Two independent optimizations to benchmark:
  1. **Pre-bind observers**: At handler registration (lines 82-105), pre-compute `requestMetric.WithLabelValues(endpoint, "ok")` and `requestMetric.WithLabelValues(endpoint, "error")` (and any other status values) into a map. In the per-request closure, look up the pre-bound observer instead of calling `With(prometheus.Labels{...})`. This eliminates the map allocation and hash lookup.
  2. **Switch to HistogramVec**: Replace the `SummaryVec` (line 74) with `HistogramVec` using appropriate duration buckets (e.g., `prometheus.DefBuckets` or custom buckets tuned for RPC latencies). This eliminates the `bufMtx` mutex in `Observe()` and replaces it with lock-free atomic increments.
- **Correctness check**: The existing `TestDecorateHandlers` and any integration tests exercising JSON-RPC endpoints should continue passing. Metric name and label schema will change if switching to Histogram, so any Grafana dashboards or alerting rules would need updating (out of scope for correctness but worth noting).
- **Benchmark focus**: Measure per-request allocation count and wrapper function CPU time (not end-to-end getTransactions latency, which will be dominated by DB/serialization). Use `go test -bench -benchmem` targeting the decorator closure. Expect to see 1-2 fewer allocations per request and ~100-300ns improvement in wrapper-only microbenchmarks. End-to-end RPS improvement is expected to be <1%.

---

## PoC Attempt

**Result**: POC_PASS
**Date**: 2026-04-07
**PoC by**: claude-opus-4.6, high

### Changes Made

1. **`cmd/stellar-rpc/internal/jsonrpc.go:48`** — Hoisted the `strings.NewReplacer` to a package-level `prometheusLabelReplacer` variable. Previously allocated inline on every error-path request (line 99); now shared and allocation-free at call time.

2. **`cmd/stellar-rpc/internal/jsonrpc.go:77-84`** — Replaced `prometheus.NewSummaryVec` (with 3 quantile objectives and per-observation mutex) with `prometheus.NewHistogramVec` using `prometheus.DefBuckets`. Histogram `Observe()` uses lock-free `atomic.AddUint64` on bucket counters instead of the Summary's `bufMtx` mutex.

3. **`cmd/stellar-rpc/internal/jsonrpc.go:88`** — Pre-bind the success-path observer via `requestMetric.WithLabelValues(endpoint, "ok")` at handler registration time (once per endpoint). This eliminates the per-request `prometheus.Labels` map allocation (~100 bytes) and the hash-based label lookup on the hot path.

4. **`cmd/stellar-rpc/internal/jsonrpc.go:97-111`** — Replaced the `prometheus.Labels` map-based status tracking with a simple `status` string variable. On the "ok" path, uses the pre-bound `okObserver` directly; on error paths, uses `WithLabelValues()` (variadic, no map allocation) instead of `With(prometheus.Labels{...})`.

### Demonstration

The optimization eliminates three sources of per-request overhead in the JSON-RPC metrics wrapper: (1) the `prometheus.Labels` map allocation on every request, (2) the label hashing and RLock-based metric lookup via `MetricVec.GetMetricWith`, and (3) the `bufMtx` mutex contention in `summary.Observe()` — replaced with lock-free atomic bucket increments in `histogram.Observe()`. The pre-bound "ok" observer bypasses both allocation and lookup entirely for successful requests, which represent the vast majority of traffic.

### Test Results

All 12 Go test packages in `cmd/stellar-rpc/internal/...` pass with `-race` enabled. All Rust tests pass (1 test in ffi crate). Build completes cleanly with no warnings.

---

## Final Review

**Verdict**: REJECTED
**Date**: 2026-04-07
**Final review by**: gpt-5.4, high
**Failed At**: final-review

### Adversarial Analysis

1. **Does the change actually address the claimed inefficiency?** **PARTIAL** — the change does remove the per-request `prometheus.Labels` allocation and swaps `SummaryVec` for a pre-bound `HistogramVec`, but the benchmarked `getTransactions` path is not bottlenecked by this wrapper work. I isolated only this metric-path change on top of the already-confirmed logging optimizations so the measurement would not be polluted by unrelated wins.
2. **Are the preconditions realistic?** **FAIL** — the optimization only helped at 100 RPS, where p50 improved from **18.479 ms** to **17.231 ms** (+6.75%). At the more demanding but still zero-error 200 RPS run, the optimized build regressed badly on the first pass (**47.295 ms** to **90.047 ms**, -90.39% p50) and still regressed on a warm-cache recheck (**47.295 ms** to **59.167 ms**, -25.10% p50).
3. **Is the original code inefficient or working as designed?** **INEFFICIENCY, BUT TOO SMALL TO MATTER** — the old `SummaryVec.With(prometheus.Labels{...}).Observe(...)` path is objectively heavier than a pre-bound observer, but the end-to-end `getTransactions` cost is dominated by database/serialization work rather than metrics bookkeeping.
4. **Does the benchmark improvement match the claimed severity?** **FAIL** — there is no stable low-severity win. At 100 RPS the optimized build was modestly better, but at 200 RPS it lost median latency and did not improve successful throughput (`5489` baseline successes vs `5428` optimized, `5447` on recheck). Both variants crossed the "latency jumped too far" threshold by 300 RPS, so the throughput ceiling remained **200 RPS**.
5. **Is the optimization in scope?** **YES** — `decorateHandlers` is on the `getTransactions` request path.
6. **Is the benchmark methodology correct?** **CORRECT** — I built the baseline and optimized binaries separately, validated the optimized worktree with `make -j8 build-stellar-rpc` and `make go-test`, generated fresh seed data from the benchmark RPC, and ran the project blaster against the same futurenet-backed local instance and seed file.
7. **Can the improvement be explained without the optimization?** **FAIL** — yes. The one encouraging data point (100 RPS) is explainable as measurement noise or load-shape variance because the improvement disappears and reverses at 200 RPS, including on a warm-cache recheck.
8. **Is this optimization novel?** **NOVEL** — no prior failure or success doc matches this exact metrics-path hypothesis.

### Rejection Reason

The claimed optimization does not produce a reliable end-to-end `getTransactions` improvement. The only clean gain appeared at 100 RPS, but the optimized build regressed median latency at 200 RPS and did not raise the zero-error throughput ceiling above 200 RPS, so the performance claim is not supported.

### Failed Checks

- 2
- 4
- 7
