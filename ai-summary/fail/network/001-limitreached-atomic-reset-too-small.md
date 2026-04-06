# H001: `limitReached` atomic reset is too small to matter for getTransactions

**Date**: 2026-04-06
**Subsystem**: network
**Severity**: Low
**Impact**: CPU contention
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

The backlog limiter's overload-log bookkeeping should not consume a meaningful fraction of `getTransactions` request time. Any optimization here would need to remove a hot-path cost large enough to measurably change `getTransactions` latency or throughput.

## Mechanism

I investigated whether `atomic.StoreUint64(&q.limitReached, 0)` in the backlog limiter defer path could create enough cacheline bouncing to matter under concurrent `getTransactions` traffic. The actual behavior does add one atomic store per admitted request, but that cost is tiny relative to the rest of the request path and does not come with extra allocations or blocking.

## Trigger

Run high-concurrency admitted `getTransactions` traffic and profile the backlog limiter's defer path for atomic contention.

## Target Code

- `cmd/stellar-rpc/internal/network/backlogQ.go:(*BacklogHTTPQLimiter).ServeHTTP:98-104` — resets `limitReached` on every admitted HTTP request
- `cmd/stellar-rpc/internal/network/backlogQ.go:(*BacklogJrpcQLimiter).Handle:134-140` — same reset on every admitted JSON-RPC request

## Evidence

Every admitted request executes the reset even if the limiter was never saturated, so the field does sit on the hot path. Under heavy concurrency that can, in theory, create cache traffic around the limiter struct.

## Anti-Evidence

The operation is a single atomic store with no heap work, no goroutine creation, and no data-dependent loop. `getTransactions` already performs much heavier ledger reads, transaction decoding, and response serialization, so this bookkeeping does not look capable of reaching the repository's measurable optimization bar.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-06
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The suspected cost is only one extra atomic store per admitted request, which is too small and too isolated to plausibly deliver a measurable `getTransactions` improvement.

### Lesson Learned

For this subsystem, viable performance work needs to target goroutine creation, timers, body buffering, or other allocation-heavy paths rather than single-atomic bookkeeping.
