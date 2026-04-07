# H003: JRPC timeout handling retains late `getTransactions` results in an unread channel

**Date**: 2026-04-07
**Subsystem**: network
**Severity**: Low
**Impact**: timeout-path heap retention / GC pressure
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once the per-method JSON-RPC timeout has fired and the client has already received `-32001`, any late `getTransactions` result should be dropped immediately. The network limiter should not keep a large abandoned response object reachable after the request has already failed.

## Mechanism

`RPCRequestDurationLimiter.Handle` returns as soon as `limitCh` fires, but the worker goroutine still unconditionally executes `requestCompleted <- res` into a buffered channel of size 1 when the downstream handler eventually finishes. For large JSON-format `getTransactions` responses, `res.data` can hold a `protocol.GetTransactionsResponse` full of `json.RawMessage` slices; if the timeout arrived after most of that work was already done, the unread channel cell keeps a multi-megabyte object alive until the goroutine, channel, and buffered element are collected. Under timeout-heavy load, that creates needless heap spikes and GC churn on requests the server has already abandoned.

## Trigger

Lower `max-get-transactions-execution-duration` enough to force timeouts after substantial work (for example 50-250ms on a dense historical range), then send many concurrent JSON-format `getTransactions` requests. Compare heap profiles and allocation rates before and after changing the timeout goroutine to discard results once `requestCtx.Err() != nil` instead of sending them into the unread buffer.

## Target Code

- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:246-264` — allocates a buffered `requestCompleted` channel and always sends `res` into it from the worker goroutine
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:272-285` — timeout path returns immediately without draining `requestCompleted`
- `cmd/stellar-rpc/internal/methods/get_transactions.go:306-375` — `getTransactions` can build large `protocol.GetTransactionsResponse` values containing many `TransactionInfo` entries
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/protocols/rpc/get_transactions.go:25-90` — `TransactionInfo` / `GetTransactionsResponse` include multiple `json.RawMessage`-heavy fields per transaction

## Evidence

The timeout path does not synchronize with the worker goroutine after returning the error. The worker still has a live reference to the eventual response and sends it into a buffered channel that no caller will ever receive from after timeout, which is enough to keep the response object reachable for an extra GC cycle. Because `getTransactions` responses are structurally large, even a handful of timed-out late completions can retain noticeable transient heap.

## Anti-Evidence

This only matters when requests are timing out, and the retained result may be smaller if cancellation is observed early inside `getTransactions`. If normal zero-error traffic dominates, the optimization has no effect on steady-state latency and will only help overload or badly tuned timeout scenarios.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated
**Failed At**: reviewer

### Trace Summary

Traced the JRPC timeout path in `RPCRequestDurationLimiter.Handle` (lines 233-303). On timeout, the caller returns at line 282 and the goroutine eventually sends `res` into the buffered channel at line 263. The hypothesis claims this channel retention keeps multi-megabyte response data alive. However, `requestResultOutput{data interface{}, err error}` is a struct of two interface values — the channel send copies ~32 bytes of pointer headers, not the underlying response data. The response data itself is held by the goroutine's local `res` variable regardless of the channel send. After the goroutine exits, both `res` and the channel become unreachable simultaneously, so Go's tracing GC collects them in the same cycle.

### Code Paths Examined

- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:246-264` — `requestResultOutput` is `{data interface{}, err error}`; channel send copies two pointer-sized interface values, not the underlying multi-MB response
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:254-264` — goroutine holds `res` as a local variable on its stack; response data is reachable via `res` until goroutine exits, regardless of channel send
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:255-260` — deferred `close(requestCompleted)` runs immediately after the send; goroutine exits promptly
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:272-285` — caller returns, dropping its reference to `requestCompleted`; after goroutine exits, the channel is unreachable and GC-eligible along with its buffer contents

### Why It Failed

The hypothesis conflates pointer retention with data retention. The channel buffer holds a copy of `requestResultOutput` — two interface values (~32 bytes of pointer headers) — not a deep copy of the multi-megabyte `GetTransactionsResponse`. The actual response data is reachable from the goroutine's local `res` variable until the goroutine exits. After the goroutine exits (which happens immediately after the channel send), both `res` on the goroutine stack and `requestCompleted` in the closure become unreachable simultaneously. Go's tracing GC collects the channel and the response data in the same cycle — there is no "extra GC cycle" of retention caused by the channel send.

The proposed fix (checking `requestCtx.Err()` before sending) would save ~32 bytes of interface-value copies per timed-out request. Even under extreme timeout load (10,000 concurrent timeouts), that is 320KB — negligible against the multi-MB live heap from the goroutines still running their handlers.

### Lesson Learned

In Go, sending a struct containing `interface{}` fields into a buffered channel copies pointer headers, not the referenced data. The dominant retention factor for abandoned goroutine results is the goroutine's own stack frame and local variables, which hold the same data regardless of the channel send. Channel-buffer "retention" only adds pointer-sized overhead when the underlying data is already pinned by the goroutine itself.
