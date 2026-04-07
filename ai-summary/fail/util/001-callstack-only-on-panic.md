# H001: CallStack Allocation Runs Only on HTTP Panic Path

**Date**: 2026-04-07
**Subsystem**: util
**Severity**: Informational
**Impact**: request latency / CPU allocation
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Successful `getTransactions` requests should traverse the HTTP and JSON-RPC wrappers without paying for panic-diagnostics work. Any util-focused optimization should therefore remove work that executes on the steady-state request path, not just after the request has already panicked and failed.

## Mechanism

I investigated whether `util.CallStack()` is a hidden hot-path cost because it allocates a `debug.Stack()` buffer, converts it to a string, and tokenizes it with `strings.FieldsFunc()`. If that work happened for every `getTransactions` request, removing it could reduce allocations and tail latency.

## Trigger

Send repeated successful `getTransactions` requests through the normal HTTP JSON-RPC endpoint without forcing any handler panic.

## Target Code

- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:(*httpRequestDurationLimiter).ServeHTTP:143-149` — invokes `util.CallStack()` only from the panic-recovery branch
- `cmd/stellar-rpc/internal/util/panicgroup.go:CallStack:100-129` — allocates and tokenizes the stack trace

## Evidence

`CallStack()` performs several allocation-heavy operations (`debug.Stack()`, `string(...)`, `FieldsFunc`, slice appends), so it would be expensive if reached for every request.

## Anti-Evidence

The only request-path call site is guarded by `if err := recover(); err != nil` inside the HTTP duration limiter goroutine. Normal `getTransactions` requests never enter that branch, and the JSON-RPC limiter used for method execution does not call `util.CallStack()` at all.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

`util.CallStack()` is only executed after a request panic, so optimizing it cannot measurably improve steady-state `getTransactions` latency or throughput.

### Lesson Learned

For util investigations on `getTransactions`, first prove the util code executes on successful requests; recovery-only code is not a viable optimization target.
