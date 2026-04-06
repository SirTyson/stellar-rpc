# H004: Client-canceled `getTransactions` requests can keep network wrapper state and current-ledger decode work alive

**Date**: 2026-04-06
**Subsystem**: network
**Severity**: Medium
**Impact**: canceled-request CPU / queue occupancy
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once the parent HTTP or JSON-RPC context is canceled, a `getTransactions` request should promptly stop consuming network-wrapper resources and should stop decoding the current ledger as soon as practical. Canceled clients should not continue burning CPU or holding request-scoped queue occupancy longer than necessary.

## Mechanism

Both duration limiters wait only on warning timers, limit timers, and the downstream completion channel; they do not directly select on the parent context's cancellation channel. At the same time, `getTransactions` only uses `ctx` when opening the read transaction and fetching ledgers, while `processTransactionsInLedger()` performs per-transaction `reader.Read`, `db.ParseTransaction`, JSON conversion, and event building without any context checks. If a client disconnects or an upstream proxy cancels the request mid-page, the wrapper goroutines, timers, and queue accounting can remain live until the in-progress ledger decode finishes.

## Trigger

Issue a large `getTransactions` request against a ledger with many transactions/events, then cancel the HTTP request before the response completes. Compare canceled-request CPU time, goroutine lifetime, and queue occupancy before and after adding parent-context fast-abort handling in the limiters and context polling inside the current-ledger processing loop.

## Target Code

- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:(*httpRequestDurationLimiter).ServeHTTP:156-196` — outer HTTP limiter waits for timeout or downstream completion, but not parent-context cancellation
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:(*RPCRequestDurationLimiter).Handle:266-302` — inner JSON-RPC limiter has the same wait pattern
- `cmd/stellar-rpc/internal/methods/get_transactions.go:(transactionsRPCHandler).getTransactionsByLedgerSequence:218-286` — passes `ctx` into DB boundaries but not into per-ledger transaction processing
- `cmd/stellar-rpc/internal/methods/get_transactions.go:(transactionsRPCHandler).processTransactionsInLedger:94-213` — performs the expensive per-transaction decode / JSON-building loop without checking cancellation

## Evidence

The network wrappers derive timeout contexts correctly, but after parent cancellation they still wait for the downstream goroutine to signal completion. `getTransactions` is especially exposed because its expensive inner loop can keep running inside one dense ledger even after the client no longer needs the result.

## Anti-Evidence

Context is still honored at several boundaries (`NewTx`, `GetLedgerRange`, `GetLedger`), so requests can stop promptly between ledgers or while blocked on I/O. The issue is most visible on large JSON pages with expensive per-transaction conversion, not on tiny or already-cached responses.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-06
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated
**Failed At**: reviewer

### Trace Summary

Traced the full request path from `BacklogHTTPQLimiter.ServeHTTP` → `httpRequestDurationLimiter.ServeHTTP` → `getTransactionsByLedgerSequence` → `processTransactionsInLedger`. Both duration limiters create `requestCtx` via `context.WithCancel(parentCtx)` (lines 139, 251), making `requestCtx` a child of the parent. When the parent is canceled (client disconnect), `requestCtx` is immediately canceled, and the downstream handler detects this at the next DB-boundary context check (`fetchLedgerData` on line 265), returning an error that signals `requestCompleted` and unblocks the select loop. The proposed fix of adding `ctx.Done()` to the select loop would create goroutine leaks and misleading backlog accounting.

### Code Paths Examined

- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:ServeHTTP:139` — `requestCtx` is derived from `req.Context()` via `context.WithCancel`, so parent cancellation propagates automatically
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:Handle:251` — same pattern: `requestCtx` is a child of `ctx`, inheriting parent cancellation
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:ServeHTTP:156-196` — select loop waits on warningCh, limitCh, requestCompleted; no explicit parent Done channel
- `cmd/stellar-rpc/internal/network/backlogQ.go:ServeHTTP:75-107` — backlog slot held synchronously until duration limiter returns; slot release depends on downstream completion
- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:258-277` — ledger iteration loop passes `ctx` to `fetchLedgerData` on each iteration, providing cancellation check at ledger boundaries
- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:94-213` — no context parameter, no cancellation check within per-transaction loop

### Why It Failed

The proposed network-layer fix (adding `ctx.Done()` to the duration limiter select loops) is counterproductive. If the select loop returns early on parent cancellation, the downstream goroutine (still running `processTransactionsInLedger`) becomes a leaked goroutine. Worse, the backlog queue slot is freed while the actual CPU work continues, causing the backlog limiter to report available capacity that doesn't exist — potentially admitting more concurrent requests than the system can actually handle, worsening overload rather than relieving it.

The existing design already handles client cancellation correctly at the network layer: `requestCtx` is a child of the parent context, so cancellation propagates to the downstream handler. The handler detects this at DB-operation boundaries (between ledgers). The only uninterruptible window is processing the remaining transactions within a single ledger — but that waste belongs to `processTransactionsInLedger` in the methods subsystem, not the network wrappers.

### Lesson Learned

The network duration limiters intentionally wait for the downstream goroutine to complete before returning, because the backlog queue limiter above them tracks actual in-flight work. Returning early from the select loop without stopping the goroutine would decouple accounting from reality. Performance improvements for mid-request cancellation should target the innermost processing loop (methods subsystem) with context polling, not the outer network wrappers.
