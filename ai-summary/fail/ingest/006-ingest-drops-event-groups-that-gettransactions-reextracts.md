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

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not an exact duplicate, but substantially subsumed by fail/ingest/003, fail/ingest/004, and reviewed/ingest/003
**Failed At**: reviewer

### Trace Summary

Traced `GetTransactionEvents()` in the stellar SDK (`go-stellar-sdk@v0.4.0/ingest/ledger_transaction.go:278-312`) and confirmed it is a near-zero-cost struct field accessor — for TransactionMetaV4 it just reads `txMeta.Events`, `txMeta.DiagnosticEvents`, and `txMeta.Operations[i].Events` as slice references with no allocation, no parsing, and no computation. The "event walk" the hypothesis claims is duplicated is simply reading pre-existing fields from an already-deserialized XDR struct. The expensive work on both paths is the subsequent marshaling, which produces different output formats for ingest vs. read and therefore cannot be shared.

### Code Paths Examined

- `go-stellar-sdk@v0.4.0/ingest/ledger_transaction.go:278-312` — `GetTransactionEvents()` is a trivial field accessor. For V4 meta: reads `txMeta.Events` (TransactionEvents), `txMeta.DiagnosticEvents`, and copies slice references from `txMeta.Operations[i].Events`. No allocation beyond the `TransactionEvents` struct itself. Near-zero CPU cost.
- `go-stellar-sdk@v0.4.0/ingest/ledger_transaction.go:264-266` — `GetDiagnosticEvents()` delegates to `t.UnsafeMeta.GetDiagnosticEvents()`, also a trivial field access.
- `cmd/stellar-rpc/internal/db/event.go:136-211` — `InsertEvents` calls `GetTransactionEvents()`, then iterates events to wrap each in a synthetic `DiagnosticEvent` with cursor-ordered TOID positioning, then marshals each via `insertEvents` for DB insertion. The output shape (ordered DB rows with topics, contract_id, cursor) is completely different from what the read path needs.
- `cmd/stellar-rpc/internal/db/transaction.go:268-316` — `ParseTransaction` calls `GetTransactionEvents()`, then `parseEvents` marshals `DiagnosticEvents` → `[][]byte`, `TransactionEvents` → `[][]byte`, `OperationEvents` → `[][][]byte` via `MarshalBinary()` on each event. This marshaling is the expensive step, not the `GetTransactionEvents()` call.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:200-219` — XDR path calls both `GetDiagnosticEvents()` and `GetTransactionEvents()`, then `MarshalBase64` on each event. Again, the marshaling is the cost, not the extraction.

### Why It Failed

1. **The "event walk" is not an expensive operation.** `GetTransactionEvents()` is a struct field accessor that reads pre-existing slice references from the already-deserialized `TransactionMeta`. For V4 metadata (current protocol), it performs zero allocations beyond the small `TransactionEvents` struct — the event slices themselves are just pointers into the existing XDR structure. Caching its output would save nanoseconds per transaction, not microseconds.

2. **The expensive work (marshaling) differs between ingest and read paths.** Ingest marshals events into DB rows with synthetic `DiagnosticEvent` wrappers, TOID cursors, and topic extraction (event.go:149-211). The read path marshals events into `[]byte` slices via `MarshalBinary()` (transaction.go:281-316) or `MarshalBase64` (get_transactions.go:206-218). These are completely different output formats — caching one doesn't eliminate the other.

3. **"Already-marshaled event byte slices" would add work to ingest.** The hypothesis's fallback proposal — caching marshaled event bytes — would require running `parseEvents()` during ingest for every transaction. This adds the full per-event `MarshalBinary()` cost to the ingest hot path regardless of whether that ledger is ever queried. This is exactly the pattern already rejected in fail/ingest/003 ("implementing the cache would ADD work to ingest, not reuse it").

4. **Completely subsumed by reviewed/ingest/003 (lazy response cache).** The already-reviewed VIABLE hypothesis caches the entire formatted `[]protocol.TransactionInfo` per ledger on first request. When that cache hits, ALL event extraction and marshaling is skipped — not just the trivial `GetTransactionEvents()` call but the expensive `MarshalBinary`/`MarshalBase64`/FFI-JSON steps too. A separate event-group cache adds zero value on top of a response cache.

### Lesson Learned

When evaluating "duplicated extraction" hypotheses, inspect the actual implementation of the extraction function. SDK accessors like `GetTransactionEvents()` often appear expensive from their name and documentation but are actually trivial field reads from pre-parsed structures. The real cost in event handling is always the subsequent marshaling/encoding step, which is format-specific and cannot be shared between write and read paths without adding new work to the write path.
