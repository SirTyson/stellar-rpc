# H003: Tip-following ingest already walks every new ledger, but hot `getTransactions` polls rebuild all transaction artifacts from scratch

**Date**: 2026-04-07
**Subsystem**: ingest
**Severity**: Low
**Impact**: CPU / hashing / marshaling / FFI overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Repeated `getTransactions` calls for the newest ledgers should reuse transaction artifacts that were already extracted when the ledger was ingested. For hot recent ledgers, the request path should not need to rebuild the SDK transaction reader, re-hash envelopes, and re-marshal result/meta/envelope/event fields every time a client polls the same fresh ledger.

## Mechanism

During ingest, the system already does the expensive per-ledger traversal needed to understand a ledger’s transactions: `InsertTransactions()` constructs a `LedgerTransactionReader` and walks every transaction, while `InsertEvents()` constructs another reader and walks every event-bearing transaction. After commit, none of that per-ledger extraction survives in memory, so `processTransactionsInLedger()` later reconstructs a fresh reader, pays the same envelope-hash setup cost, and calls `db.ParseTransaction()` to marshal all fields again for the same hot recent ledgers. A bounded ingest-fed cache of recent per-ledger transaction artifacts (for example application-order-indexed hashes plus serialized result/meta/envelope/event bytes, or prebuilt `db.Transaction` slices) could turn hot recent polls into cheap slicing and formatting instead of full re-extraction.

## Trigger

1. Run normal tip-following ingestion.
2. After each ledger close, issue a burst of `getTransactions` requests against that newest ledger, especially with dense ledgers or `format=json`.
3. Compare CPU time and allocations against a version that serves recent ledgers from an ingest-populated artifact cache instead of rebuilding `LedgerTransactionReader` and `ParseTransaction` output on every request.

## Target Code

- `cmd/stellar-rpc/internal/ingest/service.go:295-320` — ingest already invokes both transaction and event extraction for every incoming ledger.
- `cmd/stellar-rpc/internal/db/transaction.go:68-143` — write path constructs a transaction reader and walks all transactions during ingestion.
- `cmd/stellar-rpc/internal/db/event.go:76-247` — write path separately constructs a transaction reader and walks all event-bearing transactions.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:75-199` — request path rebuilds a reader and reprocesses every returned transaction from the LCM.
- `cmd/stellar-rpc/internal/db/transaction.go:238-320` — `ParseTransaction()` re-marshals result/meta/envelope/events for the read path.
- `cmd/stellar-rpc/internal/ledgerbucketwindow/ledgerbucketwindow.go:9-120` — available bounded-window primitive for retaining recent per-ledger artifacts.

## Evidence

The codebase currently pays the “understand this ledger’s transactions” cost multiple times across write and read paths: once in `InsertTransactions()`, once in `InsertEvents()`, and again for each `getTransactions` request that touches the same ledger. The only ingest-side state retained afterward is scalar sequence metadata, even though the newest ledgers are exactly the ones most likely to be polled repeatedly. Because the repository already accepts bounded per-ledger in-memory windows for fee stats, a recent transaction-artifact window is architecturally consistent and would attack a different layer than the reviewed DB fetch/decode optimizations.

## Anti-Evidence

This increases steady-state memory usage and only helps ledgers that are queried while still hot in the recent window; cold or rarely accessed ledgers still need the existing DB path. For XDR/base64-heavy workloads the win may be modest, and any implementation must keep cache fill cost from materially slowing the ingestion loop itself.
