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

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated
**Failed At**: reviewer

### Trace Summary

Traced the full request lifecycle from HTTP entry through JRPC dispatch. The `BacklogHTTPQLimiter.ServeHTTP` (global, line 75-107) executes FIRST at the HTTP layer, before `jhttp.NewBridge` parses the JSON-RPC request and dispatches to the method handler. Only then does `BacklogJrpcQLimiter.Handle` (per-method, line 109-143) run. Each limiter operates on its own separate `backlogQLimiter` struct with independent `pending` and `gauge` fields — there is no shared cache-line contention between the two limiters.

### Code Paths Examined

- `cmd/stellar-rpc/internal/jsonrpc.go:303-354` — Construction order: method limiter wraps handler first, then global limiter wraps the bridge. Runtime execution is the reverse.
- `cmd/stellar-rpc/internal/network/backlogQ.go:75-107` — `BacklogHTTPQLimiter.ServeHTTP`: atomic inc/dec on its own `pending` field, gauge inc/dec on its own gauge, CAS on its own `limitReached`. Entirely self-contained struct.
- `cmd/stellar-rpc/internal/network/backlogQ.go:109-143` — `BacklogJrpcQLimiter.Handle`: identical pattern but on a completely separate struct instance with independent atomic fields.
- `cmd/stellar-rpc/internal/config/options.go:433-491` — Default limits: global=5000, getTransactions=1000.

### Why It Failed

Three independent problems make this hypothesis non-viable:

1. **The proposed fix is architecturally impossible as described.** The hypothesis suggests "short-circuiting the global HTTP limiter for JSON-RPC calls that already passed a method-specific limiter," but the execution order is reversed: the global HTTP limiter runs BEFORE JRPC dispatch, so at the time it executes, the method is unknown. You cannot conditionally skip a check based on information that hasn't been determined yet.

2. **No shared cache-line contention between limiters.** The hypothesis claims "shared cache-line contention" from "two atomic increments," but each limiter has its own `backlogQLimiter` struct with its own `pending` uint64 and `gauge`. The atomics contend only with concurrent requests on the same limiter (inherent to any admission control), not with each other.

3. **The absolute cost is unmeasurable.** The "duplicate" bookkeeping per request is ~4 atomic operations and 2 Prometheus gauge mutations, totaling roughly 20-50ns. A typical `getTransactions` request performs SQLite queries, XDR deserialization, and JSON serialization costing 100s of µs to ms. The overhead is <0.05% of request time, well below any benchmark's noise floor and far below the "Low" severity threshold of <5% measurable improvement.

### Lesson Learned

When hypothesizing about duplicate work across layered middleware, verify the runtime execution order (not just construction order) and confirm that the layers actually share state (same atomic, same cache line). Separate struct instances with independent fields do not cause cross-layer contention regardless of how similar their code patterns look.
