# H003: Tip-following ingest already walks every new ledger, but hot `getTransactions` polls rebuild all transaction artifacts from scratch

**Date**: 2026-04-07
**Subsystem**: ingest
**Severity**: Low
**Impact**: CPU / hashing / marshaling / FFI overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Repeated `getTransactions` calls for the newest ledgers should reuse transaction artifacts that were already extracted when the ledger was ingested. For hot recent ledgers, the request path should not need to rebuild the SDK transaction reader, re-hash envelopes, and re-marshal result/meta/envelope/event fields every time a client polls the same fresh ledger.

## Mechanism

During ingest, the system already does the expensive per-ledger traversal needed to understand a ledger's transactions: `InsertTransactions()` constructs a `LedgerTransactionReader` and walks every transaction, while `InsertEvents()` constructs another reader and walks every event-bearing transaction. After commit, none of that per-ledger extraction survives in memory, so `processTransactionsInLedger()` later reconstructs a fresh reader, pays the same envelope-hash setup cost, and calls `db.ParseTransaction()` to marshal all fields again for the same hot recent ledgers. A bounded ingest-fed cache of recent per-ledger transaction artifacts (for example application-order-indexed hashes plus serialized result/meta/envelope/event bytes, or prebuilt `db.Transaction` slices) could turn hot recent polls into cheap slicing and formatting instead of full re-extraction.

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

The codebase currently pays the "understand this ledger's transactions" cost multiple times across write and read paths: once in `InsertTransactions()`, once in `InsertEvents()`, and again for each `getTransactions` request that touches the same ledger. The only ingest-side state retained afterward is scalar sequence metadata, even though the newest ledgers are exactly the ones most likely to be polled repeatedly. Because the repository already accepts bounded per-ledger in-memory windows for fee stats, a recent transaction-artifact window is architecturally consistent and would attack a different layer than the reviewed DB fetch/decode optimizations.

## Anti-Evidence

This increases steady-state memory usage and only helps ledgers that are queried while still hot in the recent window; cold or rarely accessed ledgers still need the existing DB path. For XDR/base64-heavy workloads the win may be modest, and any implementation must keep cache fill cost from materially slowing the ingestion loop itself.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not a duplicate (related to reviewed ingest/H001 LCM cache but proposes a different cache layer)
**Failed At**: reviewer

### Trace Summary

Traced the full ingest write path (`InsertTransactions` at transaction.go:68-143 and `InsertEvents` at event.go:76-247) and the read path (`processTransactionsInLedger` at get_transactions.go:75-199 calling `ParseTransaction` at transaction.go:238-292). The hypothesis claims ingest "already does the expensive per-ledger traversal" and that the read path "marshals all fields again." This is factually incorrect: the ingest write path only extracts transaction hashes and application orders for index insertion — it never calls `ParseTransaction` and never marshals Result, Meta, Envelope, or Events to bytes. The only shared work between ingest and read is `LedgerTransactionReader` construction, which is a small fraction of per-transaction cost.

### Code Paths Examined

- `cmd/stellar-rpc/internal/db/transaction.go:68-143` — `InsertTransactions` creates a reader, walks transactions, but only extracts `tx.Result.TransactionHash`, `tx.Result.InnerHash()`, and `tx.Index`. Inserts `(hash, ledger_sequence, application_order)` into SQLite. Does NOT marshal Result/Meta/Envelope to bytes.
- `cmd/stellar-rpc/internal/db/event.go:76-247` — `InsertEvents` creates a separate reader, calls `tx.GetTransactionEvents()`, and marshals events into its own DB schema format (with TOID-based ordering). This is a different output format than what `ParseTransaction` produces.
- `cmd/stellar-rpc/internal/db/transaction.go:238-292` — `ParseTransaction` marshals `ingestTx.Result.Result`, `ingestTx.UnsafeMeta`, `ingestTx.Envelope` to binary via `MarshalBinary()`, extracts diagnostic events, and constructs `db.Transaction`. None of this work is performed during ingest.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:86-129` — `processTransactionsInLedger` constructs `LedgerTransactionReader` (same as ingest), reads transactions, then calls `ParseTransaction` (NOT done during ingest) and formats results as base64/JSON.
- `cmd/stellar-rpc/internal/ingest/service.go:190-242` — `ingest()` calls `ingestLedgerCloseMeta` which fans out to `InsertLedger`, `InsertTransactions`, `InsertEvents`. After commit, the LCM is discarded. No `ParseTransaction` output is ever produced.

### Why It Failed

The hypothesis's core mechanism claim is factually wrong. It states that ingest "already does the expensive per-ledger traversal needed to understand a ledger's transactions" and that `ParseTransaction` "marshals all fields again." In reality:

1. **Ingest does NOT produce transaction artifacts.** `InsertTransactions` only extracts hashes and indexes (transaction.go:97-110). It never calls `MarshalBinary()` on Result, Meta, or Envelope. The "expensive work" that the hypothesis claims is duplicated between write and read paths does not exist on the write path.

2. **The only shared cost is `LedgerTransactionReader` construction.** Both paths call `NewLedgerTransactionReaderFromLedgerCloseMeta` and `reader.Read()`. But this reader construction is a small fraction of per-request cost — the dominant expense is `ParseTransaction`'s six `MarshalBinary` calls plus event extraction, which is exclusively a read-path operation.

3. **Implementing the cache would ADD work to ingest, not reuse it.** To populate a cache of `db.Transaction` slices, you'd need to run `ParseTransaction` during ingest for every ledger — adding the full marshaling cost to the ingest hot path regardless of whether that ledger will ever be queried via `getTransactions`. This is the opposite of what the hypothesis describes as "retaining work already done."

4. **Subsumed by reviewed ingest/H001 (LCM cache).** The already-reviewed VIABLE/Low hypothesis caches raw `LedgerCloseMeta` to skip the SQLite read + XDR deserialization, which is the actual reusable artifact from ingest. The incremental benefit of additionally pre-computing `ParseTransaction` output on top of an LCM cache is marginal and comes at significant ingest-latency and memory cost.

### Lesson Learned

When evaluating "reuse cached work" hypotheses, verify that the claimed work actually occurs on the write path. The ingest write path for transactions is deliberately minimal — it extracts only the index lookup data (hash → ledger sequence + application order) needed for `getTransaction`-by-hash queries. The full transaction artifact construction (`ParseTransaction`) is intentionally deferred to request time because it's only needed when a specific ledger's transactions are actually queried. Caching deferred work requires adding that work to the hot path, which is a fundamentally different (and costlier) proposition than retaining existing work.
