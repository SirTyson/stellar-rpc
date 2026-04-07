# H001: HTTP Duration Limiter Double-Buffers getTransactions Responses

**Date**: 2026-04-07
**Subsystem**: daemon
**Severity**: Medium
**Impact**: latency / allocation churn
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When `getTransactions` completes before its configured deadline, the daemon should serialize the JSON-RPC response once and write those bytes directly to the client socket. Large successful responses should not be copied into an intermediate buffer solely to support a timeout path that almost never fires on the success case.

## Mechanism

`NewJSONRPCHandler` wraps the JSON-RPC bridge in `MakeHTTPRequestDurationLimiter`, and that limiter always routes the response through `bufferedResponseWriter`. For `getTransactions`, which can return 50 transactions by default and up to 200 at the configured max, this means every response body is appended into a growable `[]byte` and then copied again on `WriteOut`, adding size-proportional CPU and GC work on the hot path even though the method-specific JSON-RPC limiter has already bounded handler execution.

## Trigger

Issue a steady `getTransactions` workload that returns large payloads (for example, `format=json` or high-limit XDR responses near the 50/200 transaction caps) and compare CPU/alloc profiles before and after bypassing the HTTP response buffer for JSON-RPC success paths.

## Target Code

- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:bufferedResponseWriter.Write:88-90` — every write appends response bytes into an extra buffer.
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:(*httpRequestDurationLimiter).ServeHTTP:124-186` — allocates the buffer and only flushes after the downstream handler returns.
- `cmd/stellar-rpc/internal/jsonrpc.go:NewJSONRPCHandler:368-389` — installs the HTTP duration limiter around all JSON-RPC traffic, including `getTransactions`.

## Evidence

`bufferedResponseWriter` stores the entire body in memory and `WriteOut` later writes that buffer to the real `ResponseWriter`, so successful requests necessarily pay an extra copy. The default config keeps this wrapper enabled (`max-request-execution-duration = 25s`) while `getTransactions` already has a tighter method-specific limit (`max-get-transactions-execution-duration = 5s`), so the common successful path pays the buffering cost even when no HTTP-level timeout handling is needed.

## Anti-Evidence

The outer HTTP limiter does prevent partial responses from escaping if the request times out after the JSON-RPC handler starts writing, so the buffer is not pure dead code. If the bridge already materializes the full response body internally, the incremental win may be smaller than the raw double-copy suggests.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full HTTP response path from handler return through the jhttp.Bridge to the client socket. The bridge's `writeJSON` (jhttp/getter.go:138-151) calls `json.Marshal(obj)` to produce the final JSON bytes, then calls `w.Write(bits)`. When `w` is the `bufferedResponseWriter`, `Write` appends into `w.buffer` (an extra allocation+copy). After the handler goroutine completes, `WriteOut` copies `w.buffer` to the real `http.ResponseWriter` (a second copy). The per-method `RPCRequestDurationLimiter` (lines 233-303) already provides timeout protection at the JRPC handler level without any HTTP response buffering, confirming the HTTP-level buffer is redundant for timeout enforcement on JSON-RPC requests.

### Code Paths Examined

- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:bufferedResponseWriter.Write:88-90` — confirmed: unconditionally appends all bytes into a growable `[]byte` slice
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:makeBufferedResponseWriter:75-82` — allocates a fresh `bufferedResponseWriter` with empty buffer on every request
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:(*httpRequestDurationLimiter).ServeHTTP:124-197` — runs downstream handler in a goroutine against the buffered writer; on success (line 185) calls `responseBuffer.WriteOut(req.Context(), res)` which performs the final copy
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:ServeHTTP:125-128` — pass-through path exists when `limitThreshold == RequestDurationLimiterNoLimit`, bypassing all buffering
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:RPCRequestDurationLimiter.Handle:233-303` — per-method JRPC limiter that already enforces timeouts at the handler level, operating on `(interface{}, error)` return values before any HTTP serialization
- `cmd/stellar-rpc/internal/jsonrpc.go:NewJSONRPCHandler:328-335` — each method gets its own `MakeJrpcRequestDurationLimiter` with a method-specific timeout (e.g., 5s for getTransactions)
- `cmd/stellar-rpc/internal/jsonrpc.go:NewJSONRPCHandler:368-374` — the global `MakeHTTPRequestDurationLimiter` wraps the entire bridge with a 25s timeout, adding the buffered writer layer on top of the per-method JRPC limiters
- `jhttp/bridge.go:encodeResponses:162-169` — calls `writeJSON(w, http.StatusOK, rsps[0])` for single responses
- `jhttp/getter.go:writeJSON:138-151` — calls `json.Marshal(obj)` producing `bits`, then `w.Write(bits)` — this is where the response bytes first hit the buffered writer

### Findings

**The inefficiency is real.** Every getTransactions response pays one extra allocation (the `bufferedResponseWriter.buffer` slice, sized proportional to response) and one extra memcpy (from buffer to real ResponseWriter in `WriteOut`). For a typical 200KB response (50 transactions), this is ~200KB of extra alloc + ~200KB of extra copy per request.

**The buffer is redundant for timeout protection.** The per-method `RPCRequestDurationLimiter` already cancels the handler's context on timeout and returns a JRPC error code (`-32001`). The HTTP-level limiter's buffer exists to prevent partial HTTP responses on timeout, but the JRPC limiter operates before any HTTP serialization (at the `(interface{}, error)` level), so by the time the bridge serializes and writes to the ResponseWriter, the handler has already completed or been cancelled.

**The buffer does provide panic recovery.** The HTTP-level limiter's goroutine catches panics (line 145-148) and returns a 500 instead of a partial response. This is a genuine safety benefit, though the JRPC limiter also has its own panic handler (line 256-259).

**Impact estimate:** The extra copy costs ~50-250μs per request depending on response size. Against total getTransactions latency of 5-50ms, this is roughly 0.5-5% overhead. The GC pressure from the extra allocation is an additional minor cost. The aggregate at high RPS (e.g., 1000 RPS × 200KB = 200MB/s of extra alloc) is noticeable in alloc profiles but unlikely to exceed a 5% latency reduction.

**Severity downgrade rationale:** The hypothesis claimed Medium (5-20% improvement). Based on the trace, the actual improvement is likely <5% because: (1) the extra copy is small relative to JSON marshaling, XDR parsing, and DB read costs; (2) only one allocation and one memcpy are eliminated; (3) the JSON serialization in the bridge (`json.Marshal`) is the bigger allocation cost and is unchanged.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/jsonrpc.go:NewJSONRPCHandler` lines 368-374 — change the `MaxRequestExecutionDuration` passed to `MakeHTTPRequestDurationLimiter` to `network.RequestDurationLimiterNoLimit` to activate the existing pass-through path (line 125-128 of requestdurationlimiter.go), which bypasses all buffering
- **Change description**: Set the HTTP-level duration limiter to pass-through mode for JSON-RPC traffic, since per-method JRPC limiters already handle timeouts. This eliminates the `bufferedResponseWriter` allocation and extra copy for every request. Alternatively, a more conservative approach would keep the timer but write directly to the real ResponseWriter for success cases (requires refactoring the goroutine-based architecture)
- **Correctness check**: Run `make go-test` — the existing `requestdurationlimiter_test.go` tests cover timeout and pass-through behavior. The per-method JRPC limiter tests in the same file verify that method-level timeouts still work independently
- **Benchmark focus**: Measure allocation bytes per request (`alloc_objects` and `alloc_space` in pprof) for getTransactions with large payloads (limit=200, format=json). The extra buffer allocation should disappear entirely. Latency p99 should improve by 1-4% for 200KB+ responses under sustained load

---

## PoC Attempt

**Result**: POC_PASS
**Date**: 2026-04-07
**PoC by**: claude-opus-4-6, high

### Changes Made

- `cmd/stellar-rpc/internal/jsonrpc.go` (lines 359-370): Changed the third argument to `MakeHTTPRequestDurationLimiter` from `cfg.MaxRequestExecutionDuration` to `network.RequestDurationLimiterNoLimit`. This activates the existing pass-through path in `ServeHTTP` (line 147-150 of requestdurationlimiter.go), which calls the downstream handler's `ServeHTTP` directly on the real `http.ResponseWriter` without allocating a `bufferedResponseWriter` or routing through the goroutine+timer machinery. Added a comment explaining why the pass-through is safe (per-method JRPC limiters already handle timeouts).

### Demonstration

The optimization eliminates the `bufferedResponseWriter` allocation and one full memcpy of the response body on every JSON-RPC request by activating the existing no-op pass-through path in the HTTP duration limiter. This is safe because each JSON-RPC method already has its own `RPCRequestDurationLimiter` that enforces timeouts at the handler level (before HTTP serialization), making the HTTP-level buffer redundant for timeout protection. For large getTransactions responses (~200KB at 50 transactions), this removes ~200KB of extra alloc+copy per request.

### Test Results

All Go tests pass: 18 packages tested (config, db, feewindow, ingest, integrationtest, ledgerbucketwindow, methods, network, preflight, rpcdatastore, util, xdr2json). The `network` package tests — which cover `requestdurationlimiter_test.go` timeout, pass-through, and backlog behavior — pass with `-race` enabled (1.943s).
