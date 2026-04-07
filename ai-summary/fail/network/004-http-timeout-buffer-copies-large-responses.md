# H004: The outer HTTP timeout layer copies the full getTransactions response body

**Date**: 2026-04-06
**Subsystem**: network
**Severity**: High
**Impact**: large-response allocation / copy / GC
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Large `getTransactions` responses should not require the network layer to duplicate the fully serialized JSON body in memory before sending it to the client. Once the method has safely completed within its timeout budget, the response path should avoid a second full-body copy and avoid repeated slice growth while buffering.

## Mechanism

The global HTTP duration limiter wraps the JSON-RPC bridge with `bufferedResponseWriter`, so the entire serialized HTTP body is appended into an in-memory buffer and then copied back out to the real `http.ResponseWriter` in `WriteOut`. This is especially expensive for `getTransactions`, because the endpoint can return up to 200 transactions and each JSON-format transaction includes large envelope/result/meta/event payloads. The result is at least one extra full-body copy plus additional realloc/copy churn as `append` grows the buffer.

## Trigger

Request `getTransactions` with `limit` near the configured maximum and `format=json`, preferably against ledgers with populated events so the serialized body is large. Compare latency, allocations, and RSS/GC behavior before and after bypassing full-body buffering for already-timed inner JSON-RPC responses or replacing it with a pre-sized/streaming-safe strategy.

## Target Code

- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:makeBufferedResponseWriter:75-81` — initializes an empty buffer for all HTTP responses
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:(*bufferedResponseWriter).Write:88-90` — grows the body buffer via `append`
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:(*bufferedResponseWriter).WriteOut:97-118` — writes the fully buffered body back to the real writer
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:(*httpRequestDurationLimiter).ServeHTTP:141-185` — forces the JSON-RPC bridge through the buffering path
- `cmd/stellar-rpc/internal/methods/get_transactions.go:(transactionsRPCHandler).processTransactionsInLedger:152-200` — populates the large per-transaction JSON/XDR fields returned by `getTransactions`
- `cmd/stellar-rpc/internal/methods/get_transactions.go:(transactionsRPCHandler).getTransactionsByLedgerSequence:279-286` — returns the full transaction slice to the bridge for serialization

## Evidence

`getTransactions` can return up to 200 items by default configuration, and the JSON path fills `ResultJSON`, `ResultMetaJSON`, `EnvelopeJSON`, `DiagnosticEventsJSON`, and `Events` for every returned transaction. The outer HTTP limiter buffers that already-large serialized response instead of streaming it, so response size directly amplifies network-layer allocation pressure.

## Anti-Evidence

The buffering is intentional because it prevents partial HTTP output when the outer timeout fires. Small XDR-format responses may not show a dramatic win, so the improvement should be most visible on large JSON `getTransactions` pages rather than every possible request shape.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-06
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

I traced the full HTTP request path from `httpRequestDurationLimiter.ServeHTTP` through the jhttp bridge (`writeJSON` in `jhttp/getter.go`) and into the `bufferedResponseWriter`. The bridge calls `json.Marshal(obj)` to create the fully serialized response as a single `[]byte`, then writes it in one `w.Write(bits)` call. This means the `append` growth churn claimed by the hypothesis does NOT occur — the buffer is nil and receives a single large write, allocating exactly once. However, the extra full-body copy and extra allocation ARE real: the marshaled bytes exist in the `json.Marshal` result AND in the `bufferedResponseWriter.buffer`, and then are copied again to the real `http.ResponseWriter` by `WriteOut`.

### Code Paths Examined

- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:ServeHTTP:124-197` — confirmed: creates `bufferedResponseWriter` (line 142), spawns goroutine calling downstream (line 153), then on completion calls `WriteOut` (line 185). Every non-NoLimit HTTP request goes through this path.
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:makeBufferedResponseWriter:75-81` — confirmed: buffer field is nil (not pre-allocated), only header map is initialized.
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:Write:88-90` — `append(w.buffer, buf...)`: with nil buffer and single large write, allocates once (no growth churn).
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:WriteOut:97-118` — copies headers and calls `rw.Write(w.buffer)` if context not cancelled. This is the second copy of the response body.
- `jhttp/getter.go:writeJSON:138-151` (creachadair/jrpc2@v1.3.3) — `json.Marshal(obj)` creates `bits`, sets `Content-Length` header, then single `w.Write(bits)`. Confirmed single-write pattern — no chunked/streaming writes.
- `cmd/stellar-rpc/internal/jsonrpc.go:351-357` — the global HTTP duration limiter wraps the jhttp bridge. Confirmed it is in the getTransactions call chain.
- `cmd/stellar-rpc/internal/jsonrpc.go:310-316` — per-method JRPC duration limiter ALSO exists for getTransactions. This means timeout protection is already provided at the JRPC level; the HTTP-level timeout is a second, redundant layer.
- `cmd/stellar-rpc/internal/config/options.go:362-363` — `MaxTransactionsLimit` defaults to 200. With up to 200 JSON-format transactions, responses can reach several MB.

### Findings

**The inefficiency is real but the mechanism is partially wrong:**

1. **Append growth churn does NOT occur.** The jhttp bridge marshals the full response via `json.Marshal` and writes it in a single `w.Write(bits)` call. Go's `append` on a nil slice receiving a single large input allocates exactly once with no realloc/copy churn.

2. **The extra full-body copy IS real.** The response body exists as: (a) `bits` from `json.Marshal` inside the bridge goroutine, (b) `bufferedResponseWriter.buffer` from `append`, (c) written to the real `http.ResponseWriter` via `WriteOut`. This is one extra copy compared to writing directly to the real writer.

3. **The extra allocation IS real.** Each HTTP request allocates a `bufferedResponseWriter.buffer` equal to the full response size. No `sync.Pool` or buffer reuse exists anywhere in the network package.

4. **Severity is Low, not High.** For a ~5MB JSON response (200 transactions), the extra copy takes ~0.5ms at typical memory bandwidth. Total `getTransactions` latency includes DB reads, XDR decoding, and JSON marshaling, likely 10-100ms+. The extra copy represents roughly 0.5-5% of total latency — measurable but well under the 20% threshold for High severity.

5. **GC pressure is the more significant concern.** Under high concurrency, every in-flight request holds two copies of the response body (marshal result + buffer). For 50 concurrent large requests at 5MB each, this adds ~250MB of transient GC pressure. This is harder to benchmark in isolation but could be meaningful in production.

6. **The HTTP-level buffering is architecturally redundant for timeout protection.** The per-method JRPC duration limiter (`RPCRequestDurationLimiter`) already enforces a timeout on `getTransactions`. The HTTP-level timeout only provides additional coverage for the bridge serialization/write phase, which is fast.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/network/requestdurationlimiter.go` — `bufferedResponseWriter` and `httpRequestDurationLimiter.ServeHTTP`
- **Change description**: Introduce a `sync.Pool` for `bufferedResponseWriter` instances (including their backing `[]byte` buffers) to reduce per-request allocation pressure. The pool should use `bytes.Buffer` or a pre-sized `[]byte` slice. Additionally, consider allowing `httpRequestDurationLimiter` to bypass buffering when the per-method JRPC timeout already provides protection (i.e., when the HTTP timeout ≥ the JRPC timeout). However, this architectural change is riskier; the pool approach is the simpler and safer optimization.
- **Correctness check**: `cmd/stellar-rpc/internal/network/requestdurationlimiter_test.go` has tests for HTTP duration limiting including `TestHTTPRequestDurationLimiter_Limiting`, `TestHTTPRequestDurationLimiter_NoLimiting`, `TestHTTPRequestDurationLimiter_DownstreamPanic`, and buffered writer tests. All must continue to pass.
- **Benchmark focus**: Measure allocations per request (`-benchmem`) and GC pause times under concurrent large-response load. The latency improvement will likely be <5% for single requests, but allocation count and bytes-allocated-per-op should show a clear improvement with pooling. Test with `getTransactions?limit=200&format=json` against ledgers with events.

---

## PoC Attempt

**Result**: POC_FAIL
**Date**: 2026-04-07
**PoC by**: claude-opus-4-6, high
**Failed At**: poc
**Iterations**: 1

### Failure Reason

The `sync.Pool` optimization for `bufferedResponseWriter` was implemented correctly and all 10 existing unit tests passed. However, load testing with stellar-rpc-blaster against futurenet showed no measurable improvement in throughput or latency that exceeds measurement noise.

**Benchmark comparison (getTransactions, 30s runs):**

| RPS | Metric | Baseline | Optimized | Delta |
|-----|--------|----------|-----------|-------|
| 10 | p50 | 12.1ms | 10.9ms | -9.7% |
| 10 | p99 | 38.7ms | 35.1ms | -9.1% |
| 200 | p50 | 13.1ms | 12.8ms | -1.7% |
| 200 | p99 | 41.9ms | 41.1ms | -2.0% |
| 500 | p50 | 1884ms | 2146ms | +13.9% |
| 500 | p99 | 5378ms | 6664ms | +23.9% |
| 1000 | errors | 11542 | 11098 | -3.8% |

At low load (10-200 RPS), the optimized version shows 2-10% p50 improvement, but with only 274 total requests at 10 RPS this is within statistical noise. At 200 RPS the difference narrows to ~2%, well within run-to-run variance.

At high load (500 RPS), the optimized version actually performed **worse** (p50 +14%, p99 +24%), confirming the difference at low load was measurement noise rather than a real effect. Both versions hit their throughput ceiling between 200-500 RPS with identical error behavior at 1000+ RPS.

This confirms the reviewer's severity assessment: the extra buffer copy represents <5% of total request latency, dominated by DB reads, XDR decoding, and JSON marshaling. The `sync.Pool` eliminates one allocation per request but the effect is below the noise floor of the load test methodology.

### Changes Attempted

Added a `sync.Pool` for `bufferedResponseWriter` in `cmd/stellar-rpc/internal/network/requestdurationlimiter.go`:
- `bufferedResponseWriterPool` (`sync.Pool`) with `New` function creating pre-initialized instances
- `getBufferedResponseWriter()` — retrieves from pool, resets buffer (keeping backing capacity), clears and copies headers
- `putBufferedResponseWriter()` — returns to pool, discards buffers >1MB to bound pool memory
- `ServeHTTP` modified to use pool get/put on the request-completed path (not on timeout path where the goroutine may still reference the buffer)
- All 10 existing tests in `network` package passed with the change

Changes were reverted since the optimization produced no measurable benchmark improvement.
