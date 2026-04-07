# H004: Reusing the ingested events table for `getTransactions` is not a clear performance win

**Date**: 2026-04-07
**Subsystem**: ingest
**Severity**: Low
**Impact**: event extraction / DB reuse
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

If ingest already persisted all event payloads in a form that directly matched `getTransactions` output, the endpoint should be able to reuse those rows instead of re-extracting events from transaction metadata. A viable optimization here would need to reduce both CPU and data movement relative to the current in-memory event extraction path.

## Mechanism

`InsertEvents()` does persist per-event rows with stable ordering information and transaction hashes, which initially makes the table look reusable for `getTransactions`. But the stored representation is not the same data shape that `getTransactions` needs: transaction/contract events are wrapped into synthetic `xdr.DiagnosticEvent` rows, while the endpoint still separately requires true diagnostic events from `GetDiagnosticEvents()`. Reconstructing the response would therefore add at least one SQL query plus row decoding and unwrap/re-marshal work on top of the request path rather than removing a dominant cost.

## Trigger

1. Ingest ledgers with contract events.
2. Compare the current `getTransactions` event-building path against a hypothetical version that queries `events` rows by recent ledger range or transaction hash.
3. Check whether the extra SQL and row decoding beats direct event extraction from the already-loaded `LedgerCloseMeta`.

## Target Code

- `cmd/stellar-rpc/internal/db/event.go:InsertEvents:76-248` — ingest persists ordered event rows keyed by cursor and transaction hash.
- `cmd/stellar-rpc/internal/db/event.go:insertEvents:250-289` — stored payload is `xdr.DiagnosticEvent`, not the exact response types used by `getTransactions`.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:149-216` — request path currently builds transaction/contract events and diagnostic events directly from the in-memory transaction.
- `cmd/stellar-rpc/internal/methods/get_transaction.go:BuildEventsXDRFromTransaction/BuildEventsJSONFromTransaction:133-155` — response helpers expect `TransactionEvent` / `ContractEvent` groupings, not DB event rows.

## Evidence

The event table already has two ingredients that make reuse tempting: stable chronology (`Cursor`) and transaction grouping (`transaction_hash`). That made it plausible that `getTransactions` could fetch already-ingested events instead of re-reading them from transaction metadata.

## Anti-Evidence

The table does not store the real `GetDiagnosticEvents()` output that `getTransactions` also returns, and its persisted `event_data` is not in the same grouping or type shape that response formatting expects. The current request path already has the `LedgerCloseMeta` in memory, so adding a second DB read for events is not obviously cheaper than extracting them directly.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The persisted events table is a query-oriented index for `getEvents`, not a cheap drop-in artifact cache for `getTransactions`. It lacks the exact diagnostic-event payloads `getTransactions` returns and would require extra SQL plus representation reshaping before the endpoint could use it.

### Lesson Learned

Adjacent persisted data only helps when it already matches the request path's required shape closely enough to remove work. If the request already holds the source `LedgerCloseMeta` in memory, a secondary DB lookup must eliminate a major extraction cost to be worthwhile; otherwise it just swaps one kind of work for another.
