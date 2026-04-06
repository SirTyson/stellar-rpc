# H002: Each timeout layer tracks the same deadline twice

**Date**: 2026-04-06
**Subsystem**: network
**Severity**: Medium
**Impact**: timer heap CPU / per-request allocation
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Each `getTransactions` timeout layer should maintain one deadline source per request and use that same signal both to cancel downstream work and to detect expiration. The request path should not allocate multiple runtime timers for the exact same timeout boundary.

## Mechanism

Both duration limiters create `limitCh := time.NewTimer(q.limitThreshold).C` and then separately call `context.WithTimeout(..., q.limitThreshold)`. The select loop waits on `limitCh`, while the derived context carries a second internal timer for the same deadline. Because `getTransactions` goes through both the global HTTP duration limiter and the per-method JSON-RPC duration limiter, a single request allocates two redundant deadline timers on the outer layer and two more on the inner layer.

## Trigger

Benchmark fast or medium-latency `getTransactions` traffic with default execution limits enabled, then replace the explicit `limitCh` timer with `requestCtx.Done()` or a single shared timer/cancel path. Runtime timer heap activity and request allocations should drop if this duplication is material.

## Target Code

- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:(*httpRequestDurationLimiter).ServeHTTP:130-140` — creates `limitCh` and then `context.WithTimeout` for the same duration
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:(*RPCRequestDurationLimiter).Handle:238-252` — repeats the same duplicated deadline setup on the JSON-RPC path
- `cmd/stellar-rpc/internal/jsonrpc.go:NewJSONRPCHandler:315-323` — `getTransactions` always takes the JSON-RPC duration limiter
- `cmd/stellar-rpc/internal/jsonrpc.go:NewJSONRPCHandler:355-358` — every HTTP request, including `getTransactions`, also takes the outer HTTP duration limiter

## Evidence

The code never selects on the derived context's deadline signal; it only uses the explicit `limitCh` timer. That makes the `context.WithTimeout` timer redundant for expiration detection even though it is still allocated to carry cancellation into the downstream handler.

## Anti-Evidence

The timeout context itself is still necessary so downstream work can observe cancellation. If `getTransactions` is dominated by ledger parsing and JSON conversion, the saved timer work may only produce a medium-sized improvement rather than a dramatic one.
