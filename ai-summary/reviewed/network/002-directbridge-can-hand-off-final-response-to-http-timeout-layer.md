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

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full HTTP `getTransactions` success path: request enters `httpRequestDurationLimiter.ServeHTTP` (line 124), which creates a `bufferedResponseWriter` (line 142) and spawns a goroutine (line 143) that calls `directBridge.ServeHTTP(responseBuffer, req)`. Inside `serveInternal` (directbridge.go:58-126), the handler chain (JRPC backlog → JRPC duration limiter → getTransactions handler) returns a `protocol.GetTransactionsResponse`. The bridge then marshals this (`json.Marshal(result)` at line 108), wraps it in a JSON-RPC envelope (`directBridgeSuccessResponse` at line 112), and calls `directBridgeWriteJSON(w, ...)` (line 121) which issues a single `w.Write(data)` call. The `bufferedResponseWriter.Write` (line 88-90) performs `w.buffer = append(w.buffer, buf...)` — a full memcpy of the entire response body into a new allocation. Finally, on the success path of the select loop (line 184-185), `WriteOut` copies `w.buffer` to the real `http.ResponseWriter`. The memcpy in step 3 is entirely eliminable.

### Code Paths Examined

- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:124-196` — `ServeHTTP` creates `bufferedResponseWriter` (line 142), spawns goroutine (line 143), select-loop receives on `requestCompleted` channel, then calls `responseBuffer.WriteOut` (line 185) on success
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:88-90` — `Write` does `w.buffer = append(w.buffer, buf...)`: on nil slice with large buf, allocates new backing array and copies all bytes — one ~1-5MB allocation + memcpy for large getTransactions responses
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:97-118` — `WriteOut` writes `w.buffer` to the real writer — unavoidable final copy to kernel buffers
- `cmd/stellar-rpc/internal/directbridge.go:58-126` — `serveInternal` computes full response before any write: handler returns at line 93, marshal at line 108, envelope at line 112, single write at line 121
- `cmd/stellar-rpc/internal/directbridge.go:210-216` — `directBridgeWriteJSON` sets Content-Type/Content-Length headers and issues one `w.Write(data)` — confirmed single-write property
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:233-303` — JRPC `RPCRequestDurationLimiter.Handle` uses typed `requestResultOutput` channel (line 246-250) without any buffered writer — proves the typed-handoff pattern works within this codebase
- `cmd/stellar-rpc/internal/jsonrpc.go:359-365` — Wiring confirms `directBridge` is wrapped by `MakeHTTPRequestDurationLimiter` with the global HTTP timeout (25s default); all HTTP traffic goes through `bufferedResponseWriter`

### Findings

The inefficiency is real and on the hot path. Every successful `getTransactions` HTTP response undergoes a full-body memcpy from the bridge's final `json.RawMessage` into `bufferedResponseWriter.buffer`, followed by a second copy from `buffer` into the real `http.ResponseWriter`. The first copy exists solely because the HTTP duration limiter uses the generic `http.Handler` interface, requiring a fake writer to capture output. For a 200-transaction JSON response (~1-5MB), this unnecessary memcpy costs ~50-500µs on modern hardware.

**Distinction from prior work:**
- Fail H004 (sync.Pool): targeted allocation reuse for the destination buffer but still performed the full memcpy. Saved only allocation time (~µs), not copy time (~100s of µs). Its failure does not preclude this proposal.
- Fail H024 (Content-Length preallocation): NOT_VIABLE because the path is already single-write — no slice growth to eliminate. Different mechanism.
- Fail H015 (bypass entire outer HTTP timeout): meta-rejected because three component PoCs failed, but none of those PoCs targeted the body memcpy itself.
- The JRPC duration limiter (`RPCRequestDurationLimiter.Handle`, line 233-303) already uses a typed channel handoff (`requestResultOutput` struct), proving this pattern is sound within the same codebase.

**Correctness analysis:**
- The typed handoff preserves timeout semantics: the select-loop still races the completion channel against the limit timer, returning 504 on timeout or forwarding the typed response on success.
- The `directBridge` is internal and the only downstream handler for this specific limiter instance — no generic `http.Handler` compatibility concern.
- Error paths (handler errors, parse failures, panics) would still need to write HTTP error responses directly to the real writer or through a lightweight error channel — these are small responses where buffering cost is negligible.
- The channel send/receive provides the necessary happens-before guarantee for cross-goroutine data access.

**Implementation note:** The same copy elimination can be achieved more simply by modifying `bufferedResponseWriter.Write` to take ownership of the buffer on first write (`w.buffer = buf` instead of `append`) when `w.buffer` is nil. This avoids the full interface refactor while achieving identical savings, since `directBridgeWriteJSON` is single-write and does not reuse the buffer after `Write` returns. The PoC agent should consider this simpler approach.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/network/requestdurationlimiter.go` — modify `bufferedResponseWriter.Write` (lines 88-90)
- **Change description**: The simplest approach is a zero-copy first-write path: when `w.buffer == nil`, assign `w.buffer = buf` directly instead of `append(w.buffer, buf...)`. Add a boolean flag (e.g., `borrowed bool`) to track ownership. On subsequent writes, allocate a new buffer and copy both existing and new data (fallback to copy semantics). This eliminates one full-body memcpy and one allocation on the single-write hot path without changing the limiter-bridge interface. The more invasive typed-handoff approach (changing to a channel-based response object) is also correct but unnecessary for achieving the same savings.
- **Correctness check**: Existing tests in `cmd/stellar-rpc/internal/network/requestdurationlimiter_test.go` — all tests should pass. Verify multi-write scenarios still work via the fallback path. Verify panic-recovery path (which doesn't read `w.buffer`) is unaffected. The `io.Writer` contract does not guarantee callers won't reuse the slice after Write returns, but `bufferedResponseWriter` is an unexported internal type with a single usage site, and `directBridgeWriteJSON`'s `data` is a local that goes out of scope immediately — document this assumption.
- **Benchmark focus**: Measure response-path allocations (`-benchmem`) and p50/p95 latency for large `getTransactions` responses (200 transactions, JSON format). Expect one fewer ~1-5MB allocation and one fewer ~1-5MB memcpy per request. Savings of ~50-500µs represent ~0.5-5% of typical getTransactions latency. Use the highest stable zero-error RPS to maximize signal-to-noise ratio against the ±30% futurenet variance.
