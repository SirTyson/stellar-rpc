# H003: Global and Method Queue Limiters Duplicate Inflight Accounting for getTransactions

**Date**: 2026-04-07
**Subsystem**: daemon
**Severity**: Low
**Impact**: atomic contention / metrics overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

For a `getTransactions` request stream, the daemon should perform the minimum admission-control bookkeeping needed to enforce the effective queue limit. Requests that are already passing through a method-specific inflight gate should not also pay a second shared inflight counter and gauge update unless that second limiter is providing distinct enforcement for the same workload.

## Mechanism

`NewJSONRPCHandler` wraps `getTransactions` first with `MakeJrpcBacklogQueueLimiter` and later wraps the entire bridge with `MakeHTTPBacklogQueueLimiter`, so every request performs two atomic increments, two decrements, two gauge mutations, and two limit-reached CAS checks before the method body runs. On single-endpoint or mostly-`getTransactions` load, the tighter method-specific limit already governs admission, making the global limiter largely redundant while still adding shared cache-line contention on every request.

## Trigger

Benchmark a high-concurrency `getTransactions` workload that stays below both queue limits, then compare CPU profiles or RPS after short-circuiting the global HTTP limiter for JSON-RPC calls that already passed a method-specific limiter.

## Target Code

- `cmd/stellar-rpc/internal/jsonrpc.go:303-307` — installs the per-method JSON-RPC backlog limiter.
- `cmd/stellar-rpc/internal/jsonrpc.go:350-354` — wraps the whole bridge in a second HTTP backlog limiter.
- `cmd/stellar-rpc/internal/network/backlogQ.go:(*BacklogJrpcQLimiter).Handle:109-143` — per-method atomic/gauge bookkeeping.
- `cmd/stellar-rpc/internal/network/backlogQ.go:(*BacklogHTTPQLimiter).ServeHTTP:75-107` — global atomic/gauge bookkeeping repeated for the same request.

## Evidence

The default limits are `1000` for `getTransactions` and `5000` globally, so a dedicated `getTransactions` load test will almost always hit the method-specific gate first while still paying both bookkeeping layers. Both implementations mutate shared atomics and gauges on every accepted request, which is exactly the kind of low-level contention that shows up only under concurrency-heavy throughput testing.

## Anti-Evidence

The global limiter still protects mixed-method traffic and total server concurrency, so it is not universally redundant. The measurable win may therefore depend on making the fast path more selective rather than removing the global limiter outright.
