# H003: Successful `getTransactions` requests still pay a redundant outer HTTP timeout wrapper

**Date**: 2026-04-07
**Subsystem**: network
**Severity**: Low
**Impact**: duplicate timeout/buffering overhead on successful requests
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once `getTransactions` already has a method-specific backlog limiter and a method-specific execution timeout, successful requests should not also allocate a second HTTP timeout goroutine, timer set, context, and full-response buffer just to protect the in-process bridge's encode-and-write phase. The common success path should use the cheapest wrapper stack that still enforces the intended timeout semantics.

## Mechanism

`NewJSONRPCHandler` wraps the per-method `getTransactions` handler with `MakeJrpcRequestDurationLimiter`, then wraps the whole HTTP bridge again with `MakeHTTPRequestDurationLimiter`. Every successful `getTransactions` call therefore pays for the outer HTTP duration limiter's timers, goroutine, context, channel, `bufferedResponseWriter`, and extra body copy even though the expensive method body is already guarded by the inner 5s JSON-RPC timeout and the outer 25s limit mostly covers in-process bridge serialization plus the final HTTP write.

## Trigger

Benchmark large successful `getTransactions` responses (`format=json`, high `limit`) with the current stack and compare against a variant that bypasses the outer HTTP duration limiter for JSON-RPC POST requests whose method already has an inner execution limit. Watch admitted-request latency, allocation volume, and bytes copied per request; the issue is present if the wrapper-only savings remain measurable without changing method results or timeout behavior for genuinely long-running handlers.

## Target Code

- `cmd/stellar-rpc/internal/jsonrpc.go:291-323` — `getTransactions` is already wrapped by `MakeJrpcRequestDurationLimiter`
- `cmd/stellar-rpc/internal/jsonrpc.go:337-361` — the full bridge is wrapped again by `MakeHTTPRequestDurationLimiter`
- `cmd/stellar-rpc/internal/config/options.go:525-531` — global HTTP request timeout defaults to 25s
- `cmd/stellar-rpc/internal/config/options.go:575-579` — `getTransactions` method timeout defaults to 5s
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:124-194` — the outer HTTP limiter allocates timers, a derived context, a goroutine, and a buffered response writer for every admitted request
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:69-119` — `bufferedResponseWriter` captures the full response body before copying it back to the real writer

## Evidence

The outer HTTP timeout wrapper is always on the success path, not just the overload path. Prior investigation already showed that the body buffer copy by itself is real but too small to justify a standalone pooling fix; removing the whole outer wrapper for timed JSON-RPC methods is a different optimization because it also eliminates the extra goroutine, timers, context, channel, and buffer allocation that every successful request currently pays.

## Anti-Evidence

The outer wrapper still protects code outside the method handler, including bridge serialization, panic handling, and the final HTTP write, so any bypass must preserve those semantics for the endpoints that actually need them. The likely win is modest because the bridge round-trip and method body still dominate total `getTransactions` cost.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — mechanistically distinct from prior individual fixes (H002 timer duplication, H003-fail timer leak, H004 buffer pooling)
**Failed At**: reviewer

### Trace Summary

Traced the full request path through `httpRequestDurationLimiter.ServeHTTP` (lines 124-197) and confirmed the per-request overhead: goroutine creation, buffered channel allocation, `context.WithTimeout`, two `time.NewTimer` calls, `bufferedResponseWriter` allocation (header map + body buffer), one extra `memcpy` of the full response body in `WriteOut`, and header map copy-back. For `getTransactions`, this stacks on top of the inner `RPCRequestDurationLimiter.Handle` (lines 233-303) which provides its own goroutine, timers, context, and channel. The hypothesis proposes bypassing the entire outer wrapper to eliminate all of this overhead at once.

### Code Paths Examined

- `requestdurationlimiter.go:ServeHTTP:124-197` — outer HTTP wrapper: allocates `bufferedResponseWriter` (line 142), spawns goroutine (line 143), creates `context.WithTimeout` (line 139), two `time.NewTimer` calls (lines 132, 136), buffered channel (line 138). On success, calls `WriteOut` which copies the buffered body to the real writer (line 185).
- `requestdurationlimiter.go:makeBufferedResponseWriter:75-81` — allocates header map, nil buffer. Buffer grows via single `append` when `Write` is called.
- `requestdurationlimiter.go:WriteOut:97-118` — iterates and copies headers, then writes body to real `http.ResponseWriter`. The body copy is the largest single cost for large responses.
- `requestdurationlimiter.go:Handle:233-303` — inner JRPC wrapper: identical pattern (goroutine, timers, context, channel) but wraps only the handler function, not the bridge serialization/write phase.
- `jsonrpc.go:316-323` — per-method JRPC duration limiter wraps `queueLimiter.Handle` with 5s timeout for getTransactions.
- `jsonrpc.go:355-361` — global HTTP duration limiter wraps the entire jhttp bridge with 25s timeout.
- `jhttp/getter.go:writeJSON:138-151` — bridge writes response via `json.Marshal(obj)` → single `w.Write(bits)` call. With the outer wrapper, `w` is `bufferedResponseWriter`; without, it would be the real `http.ResponseWriter`.
- `jhttp/bridge.go:encodeResponses:162-168` — for single non-batch responses, passes `rsps[0]` (already `json.RawMessage`) to `writeJSON`.

### Why It Failed

Three prior PoC attempts have systematically tested the major individual cost components of this same outer HTTP wrapper overhead and all produced correct code changes that failed to demonstrate measurable benchmark improvement:

1. **H002 (timer duplication)**: Replaced `context.WithTimeout` with `context.WithCancel` to eliminate redundant timers. Savings: ~200-400ns/request. POC_FAIL — latency differences attributable to run-to-run variance (±30%).

2. **H003-fail (timer leak)**: Retained `*time.Timer` handles and added `Stop()` on completion paths. Savings: ~200-400ns/request. POC_FAIL — second-run consistently outperformed first regardless of which binary, confirming environmental dominance.

3. **H004 (buffer allocation)**: Added `sync.Pool` for `bufferedResponseWriter`. Savings: one allocation per request. POC_FAIL — at 200 RPS, p50 improved 2.3% (13.1→12.8ms), within noise; at 500 RPS, optimized version was actually 14% worse.

This hypothesis proposes combining all individual savings by bypassing the entire wrapper. The per-request cost breakdown:
- Goroutine creation + cleanup: ~2-3μs
- Two `time.NewTimer` allocations: ~200-400ns
- `context.WithTimeout` allocation: ~100-200ns
- Buffered channel creation: ~100-200ns
- `bufferedResponseWriter` struct + header map: ~500ns
- Response body buffer copy (1-5MB response): ~100-500μs

Total: ~103-504μs per request, dominated by the body copy for large responses. Against typical getTransactions p50 latency of 3-13ms, this represents at most ~4-5% — straddling the Low/Informational boundary. However, the benchmark methodology's noise floor (±10-30% as established by all three prior PoCs) prevents reliably detecting improvements below ~10%, making even the combined savings unmeasurable with available infrastructure.

The hypothesis explicitly states "the likely win is modest" and the prior evidence confirms this assessment empirically. The combined savings cannot exceed the sum of individually unmeasurable parts when measured against the same noise floor.

### Lesson Learned

When three independent PoC attempts targeting the largest individual cost components of an overhead source all fail to produce measurable improvement, a "bypass everything" hypothesis targeting the same overhead source is unlikely to succeed — the combined savings are bounded by the sum of individually unmeasurable components, and the benchmark noise floor that prevented detecting each component individually also prevents detecting their sum. Future hypotheses in the network subsystem should target overhead sources that are individually large enough to exceed the ±10-30% benchmark variance floor, rather than attempting to aggregate sub-threshold micro-optimizations.
