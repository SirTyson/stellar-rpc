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
