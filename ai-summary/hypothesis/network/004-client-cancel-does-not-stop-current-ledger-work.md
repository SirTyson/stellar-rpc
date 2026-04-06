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
