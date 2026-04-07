# H023: Outer HTTP timeout can discard large late `getTransactions` success bodies

**Date**: 2026-04-07
**Subsystem**: network
**Severity**: Low
**Impact**: timeout-path buffered-body waste
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

If a large `getTransactions` success response is going to miss the HTTP deadline, the outer timeout wrapper should stop buffering that body as soon as the timeout fires. Otherwise the server would spend CPU and memory copying a multi-megabyte response that will never be written to the client.

## Mechanism

`httpRequestDurationLimiter` returns `504` on `limitCh` and only consults `ctx.Err()` later in `WriteOut`, so at first glance it looks like a late `getTransactions` success body could still be copied into `bufferedResponseWriter` after the deadline. If that were the active deadline for this method, a context-aware `Write` fast-fail could save a large amount of useless copying on timed-out responses.

## Trigger

Send large JSON-format `getTransactions` requests and try to force the outer HTTP timeout to fire while the response body is still being produced, then compare heap/CPU before and after teaching `bufferedResponseWriter.Write` to stop accepting bytes once the request context is canceled.

## Target Code

- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:(*httpRequestDurationLimiter).ServeHTTP:124-196` — outer HTTP timeout wrapper
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:(*bufferedResponseWriter).Write:88-90` — unconditional body copy into the timeout buffer
- `cmd/stellar-rpc/internal/jsonrpc.go:258-264` — `getTransactions` method-specific timeout configuration
- `cmd/stellar-rpc/internal/config/options.go:528-532` — global HTTP timeout default
- `cmd/stellar-rpc/internal/config/options.go:576-580` — `getTransactions` timeout default

## Evidence

`bufferedResponseWriter.Write` is not context-aware, and the outer HTTP limiter’s timeout path does not stop the downstream goroutine before it returns. In isolation that makes a discarded-large-body hypothesis look plausible.

## Anti-Evidence

The default timeout ordering makes this path uninteresting for `getTransactions`: the per-method JSON-RPC timeout is **5s** while the outer HTTP timeout is **25s**, so `getTransactions` times out at the inner limiter long before the outer wrapper can encounter a large late success body. Once the inner limiter fires, the bridge returns a small JSON-RPC error instead of a giant success payload, so the outer wrapper never becomes the place where large-body timeout waste is paid for this endpoint.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

`getTransactions` is protected by a much shorter per-method timeout than the global HTTP timeout, so the large-success-body scenario does not occur in the normal endpoint path. The outer wrapper only sees the small timeout error response after the inner limiter has already aborted the method.

### Lesson Learned

For `getTransactions`, timeout-path network work is dominated by the **inner JSON-RPC limiter**, not the outer HTTP limiter. Any timeout optimization for large abandoned results must target the per-method JRPC layer, or it is looking at the wrong deadline.
