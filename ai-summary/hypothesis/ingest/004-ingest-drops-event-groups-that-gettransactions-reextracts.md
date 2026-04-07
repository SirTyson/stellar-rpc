# H004: Ingest drops per-transaction event groups that `getTransactions` re-extracts on the next hot read

**Date**: 2026-04-07
**Subsystem**: ingest
**Severity**: Low
**Impact**: event extraction / allocation / hot-ledger CPU waste
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

For a freshly ingested ledger, `getTransactions` should be able to reuse the transaction-event groupings ingest already extracted while populating the events table. The first hot read should not need to recompute transaction events, contract events, and diagnostic-event groupings if ingest just produced them for the same ledger.

## Mechanism

`eventHandler.InsertEvents()` calls `tx.GetTransactionEvents()` for every transaction and receives the full `ingest.TransactionEvents` structure, but uses it only long enough to flatten rows for the `events` table. `getTransactions` later extracts events again: the JSON path rebuilds event byte slices through `ParseTransaction()` and `BuildEventsJSONFromTransaction()`, while the XDR path calls `GetDiagnosticEvents()` and `GetTransactionEvents()` again before encoding. A recent ingest-fed cache of per-ledger event groups (or even already-marshaled per-transaction event byte slices) could let the first post-close request reuse that extracted structure instead of repeating the event walk.

## Trigger

1. Ingest ledgers that contain many contract and diagnostic events.
2. Issue `getTransactions` requests for the newest ledger in both `xdr` and `json` formats.
3. Compare the current path against a version that serves event data from an ingest-populated recent cache before calling `GetTransactionEvents()` / `GetDiagnosticEvents()` again.

## Target Code

- `cmd/stellar-rpc/internal/db/event.go:123-244` — `InsertEvents()` already extracts `allEvents := tx.GetTransactionEvents()` for every ingested transaction.
- `cmd/stellar-rpc/internal/db/transaction.go:268-316` — `ParseTransaction()` immediately extracts and marshals transaction, contract, and diagnostic events again.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:168-181` — JSON path rebuilds diagnostic and grouped events for every request.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:200-217` — XDR path calls `GetDiagnosticEvents()` and `GetTransactionEvents()` again before base64 encoding.
- `cmd/stellar-rpc/internal/methods/get_transaction.go:133-155` — response helpers can already consume pre-marshaled event byte slices once they exist.

## Evidence

The event extraction work is demonstrably duplicated across write and read paths. The write side already computes `ingest.TransactionEvents` as part of normal ingest, but none of that extracted grouping survives long enough to help the first hot `getTransactions` call for the same ledger. This is a narrower and cheaper reuse target than full response caching because it keeps formatting on-demand while eliminating repeated event discovery.

## Anti-Evidence

The main transaction result/meta/envelope encoding costs remain, so the overall speedup is bounded and strongest only on event-heavy ledgers. The implementation must also confirm that the cached event-group structure exactly matches what `GetDiagnosticEvents()` and the response helpers would have produced.
