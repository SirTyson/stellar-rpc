# H002: `directBridge` can hand off the final response to the HTTP timeout layer without `bufferedResponseWriter`

**Date**: 2026-04-07
**Subsystem**: network
**Severity**: Low
**Impact**: response buffering / allocation pressure
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once `directBridge` has produced the final JSON-RPC HTTP response for a successful `getTransactions` call, the outer HTTP timeout layer should only need to decide whether to send or drop that already-materialized response. It should not have to replay the response through a fake `http.ResponseWriter` that clones headers and copies the full body into an intermediate buffer first.

## Mechanism

`httpRequestDurationLimiter.ServeHTTP` is still written for a generic downstream handler, so it builds a `bufferedResponseWriter`, clones the current header map, and makes the downstream goroutine call `WriteHeader`/`Write` into that buffer. But the current downstream is the internal `directBridge`, and its success path already computes the complete immutable `json.RawMessage` before `directBridgeWriteJSON` performs a single write. If the direct bridge returned `{status, headers, body}` on the completion channel instead of writing into a fake writer, the timeout layer could preserve the same timeout semantics while eliminating the extra response-body copy on every large successful `getTransactions` reply.

## Trigger

Run large single-request `getTransactions` calls with `xdrFormat=json` and `pagination.limit=200`, then compare baseline against a version where `directBridge.serveInternal` returns a typed response object to the timeout goroutine instead of writing through `bufferedResponseWriter`. Measure allocations and p50/p95 latency on the success path.

## Target Code

- `cmd/stellar-rpc/internal/jsonrpc.go:359-370` — the global HTTP duration limiter still wraps the direct bridge
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:69-118` — `bufferedResponseWriter` clones headers and copies the body into `w.buffer`
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:138-185` — the HTTP timeout goroutine currently communicates completion only by writer side effects
- `cmd/stellar-rpc/internal/directbridge.go:58-125` — `serveInternal` computes the whole response before it reaches `directBridgeWriteJSON`
- `cmd/stellar-rpc/internal/directbridge.go:210-216` — `directBridgeWriteJSON` performs one terminal body write from an already-complete `json.RawMessage`

## Evidence

The current bridge success path is already single-shot and fully materialized before the write call, so the generic buffering layer is doing redundant in-process work just to delay committing bytes to the real socket. Because `directBridge` is internal to this package, a typed handoff does not need to rely on the looser `io.Writer` ownership assumptions explored in earlier generic buffering investigations.

## Anti-Evidence

This only helps the HTTP JSON-RPC path backed by `directBridge`; it does nothing for generic HTTP handlers or timeout error paths. The interface refactor is also more invasive than a local `bufferedResponseWriter` tweak because it changes how the duration limiter and bridge communicate completion.
