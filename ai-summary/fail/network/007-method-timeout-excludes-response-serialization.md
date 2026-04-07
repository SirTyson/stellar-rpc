# H004: `MaxGetTransactionsExecutionDuration` stops before the expensive response serialization phase

**Date**: 2026-04-07
**Subsystem**: network
**Severity**: Medium
**Impact**: timeout-budget CPU / allocation waste
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

The configured `max-get-transactions-execution-duration` should bound the whole cost of producing a `getTransactions` reply that the client waits on, including serializing the returned response object into JSON-RPC bytes. A request that has already consumed its 5s method budget should not continue spending extra CPU and heap on response encoding outside that budget.

## Mechanism

The JRPC duration limiter wraps only `queueLimiter.Handle`, which returns a Go `protocol.GetTransactionsResponse`. After that limiter has already declared success, `jrpc2.Server.invoke` still performs `json.Marshal(v)` on the full result, and the bridge then wraps and writes that JSON outside the 5s method timer. Because the outer HTTP timeout is 25s by default, a large `getTransactions` page can continue burning CPU and allocations well after the method-specific deadline has effectively been exceeded.

## Trigger

Drive `getTransactions` close to its 5s handler budget using large ledgers and `format=json`, then compare total request time and CPU after moving response serialization inside the method-specific timeout boundary. The issue is present if successful responses can exceed the 5s `getTransactions` budget without tripping the inner limiter, but fail only at the much later 25s HTTP timeout.

## Target Code

- `cmd/stellar-rpc/internal/jsonrpc.go:316-323` — the per-method `MakeJrpcRequestDurationLimiter` wraps only the handler/backlog path
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:(*RPCRequestDurationLimiter).Handle:233-300` — returns the raw handler result as soon as the downstream call completes
- `github.com/creachadair/jrpc2@v1.3.5/server.go:Server.invoke:370-389` — serializes the returned result with `json.Marshal(v)` after the JRPC limiter has finished
- `github.com/creachadair/jrpc2@v1.3.5/jhttp/bridge.go:Bridge.serveInternal:126-149` — performs additional response wrapping after the method timer is no longer active
- `cmd/stellar-rpc/internal/config/options.go:522-531` — global HTTP timeout defaults to 25s
- `cmd/stellar-rpc/internal/config/options.go:575-580` — `getTransactions` method timeout defaults to 5s

## Evidence

The code path clearly separates "handler completed" from "response serialized and written". `getTransactions` is unusually exposed because its response object can contain hundreds of transactions plus multiple prebuilt JSON blobs per transaction, making the post-handler serialization phase materially larger than for small methods.

## Anti-Evidence

Requests that finish well under 5s or that use smaller XDR-format payloads may not notice the gap. The current outer HTTP limiter still prevents unbounded runtime, so the issue shows up as wasted work and budget slippage rather than an infinite hang.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated
**Failed At**: reviewer

### Trace Summary

Traced the full request path from `RPCRequestDurationLimiter.Handle` (requestdurationlimiter.go:233-300) through `jrpc2.Server.invoke` (server.go:379-395) and into the jhttp bridge (bridge.go:126-149). Confirmed that `json.Marshal(v)` at server.go:395 and the bridge's response encoding both execute after the per-method duration limiter returns. However, this does not constitute a performance inefficiency because the serialization work is identical regardless of where the timeout boundary falls, and the response struct heavily uses `json.RawMessage` fields that are essentially zero-cost byte copies during marshaling.

### Code Paths Examined

- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:RPCRequestDurationLimiter.Handle:233-300` — confirmed handler returns `(interface{}, error)` and the timer stops at this point
- `jrpc2@v1.3.3/server.go:invoke:379-395` — `json.Marshal(v)` serializes the returned struct after the limiter has finished (actual version is v1.3.3, not v1.3.5 as hypothesis claims)
- `go-stellar-sdk@v0.4.0/protocols/rpc/get_transactions.go` — `GetTransactionsResponse` struct: `TransactionDetails` uses `json.RawMessage` for all heavy fields (EnvelopeJSON, ResultJSON, ResultMetaJSON, DiagnosticEventsJSON, ContractEventsJSON) — these are pre-serialized in the handler and merely copied during Marshal
- `cmd/stellar-rpc/internal/methods/get_transactions.go:152-200` — handler pre-serializes JSON via `transactionToJSON()` and `jsonifySlice()`, storing results as `json.RawMessage`
- `jrpc2@v1.3.3/jhttp/bridge.go:serveInternal:126-149` — marshals the full JSON-RPC response envelope, then writes to the HTTP response

### Why It Failed

1. **No performance improvement is possible.** The total CPU work per request is `handler_time + serialization_time` regardless of where the timeout boundary is drawn. Moving serialization inside the timeout does not eliminate or reduce any computation — it merely changes which requests get killed at the boundary.

2. **The serialization cost is minimal.** The heavy payload fields (`EnvelopeJSON`, `ResultJSON`, `ResultMetaJSON`, `DiagnosticEventsJSON`, `ContractEventsJSON`) are all `json.RawMessage` in the `TransactionDetails` struct. `json.Marshal` on `json.RawMessage` performs a compact validation pass (O(n) scan), not a full re-encoding. The only real serialization work is encoding simple scalar fields (hashes, ints, bools) and copying pre-serialized byte slices. For 200 transactions, this is likely 1-10ms — negligible against a 5s handler budget.

3. **Moving serialization inside the timeout would be counter-productive.** Requests completing handler work in 4.95s currently succeed because serialization (say 5ms) happens outside the 5s boundary. With serialization inside the timeout, these requests would be killed, wasting the 4.95s of handler work already done and forcing clients to retry — actually increasing total server load.

4. **Implementation requires third-party changes.** The `json.Marshal(v)` call is in `jrpc2.Server.invoke`, which is the third-party `creachadair/jrpc2` library. Pre-serializing in the handler to work around this would add complexity with no measurable benefit.

### Lesson Learned

The per-method timeout is correctly scoped to handler execution time, not total request processing. Post-handler serialization using `json.RawMessage` fields is essentially a copy operation, not a serialization bottleneck. Timeout-boundary placement is a correctness/policy concern, not a performance optimization lever — the total work is invariant under boundary movement.
