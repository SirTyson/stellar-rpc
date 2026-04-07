# H003: Ingest learns exactly which recent ledgers are empty, but hot `getTransactions` polls still ask SQLite

**Date**: 2026-04-07
**Subsystem**: ingest
**Severity**: Low
**Impact**: planner SQL / recent-tip fixed overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

For the newest retained ledgers, `getTransactions` should be able to know exactly which ledgers contain any transactions without issuing a `SELECT DISTINCT ledger_sequence ...` query first. Recent tip pollers should not hit SQLite for a presence/absence signal that ingest already observed while indexing those same ledgers.

## Mechanism

`InsertTransactions()` computes `txCount := lcm.CountTransactions()` before it does any write-side work and immediately knows whether the just-ingested ledger is empty or non-empty, but that exact signal is discarded after the write returns. `getTransactionsByLedgerSequence()` therefore always calls `GetLedgerSequencesWithTransactions()` even for near-tip requests that only need the last few ledgers. A bounded ingest-fed `LedgerBucketWindow` that records recent non-empty ledgers (or recent tx counts used only as exact presence bits) could answer recent planning without SQL and hand the handler the exact ledger sequence list it needs.

## Trigger

1. Run tip-following ingest over a recent range that includes a mix of empty and non-empty ledgers.
2. Send `getTransactions` requests with `startLedger` near the latest retained ledger and `limit=1..10`.
3. Compare the current planner query against a version that checks a recent ingest-maintained non-empty-ledger window before falling back to `GetLedgerSequencesWithTransactions()`.

## Target Code

- `cmd/stellar-rpc/internal/db/transaction.go:InsertTransactions:73-148` — ingest computes `txCount` and returns immediately for zero-transaction ledgers.
- `cmd/stellar-rpc/internal/ingest/service.go:ingestLedgerCloseMeta:295-320` — write-side ingest retains no recent transaction-presence metadata after calling the writers.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:326-337` — every request still runs the transaction-index planner query.
- `cmd/stellar-rpc/internal/ledgerbucketwindow/ledgerbucketwindow.go:25-120` — existing contiguous-ledger window primitive can hold recent exact presence metadata with built-in eviction and range semantics.

## Evidence

This information is available at ingest time for free: `txCount == 0` already decides whether `InsertTransactions()` does any work at all. Unlike the previously rejected tx-count batching idea, this proposal does not try to predict how many ledgers a page will need; it only replaces the recent `DISTINCT ledger_sequence` lookup with exact ingest-produced presence data for the same recent ledgers.

## Anti-Evidence

The optimization helps only within the recent in-memory window; historical queries still need the SQL planner. SQLite's `ledger_sequence` index may already make the `DISTINCT` query quite cheap, so the win is likely limited to high-QPS small-limit polling once larger decode/formatting costs have been reduced.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not a duplicate (distinct from ingest/002 which targeted batch-size capping, not DISTINCT query replacement), though the root rejection reason overlaps
**Failed At**: reviewer

### Trace Summary

Traced the `getTransactionsByLedgerSequence` path (get_transactions.go:277-395) and confirmed the `GetLedgerSequencesWithTransactions` call at line 329-331. This executes `SELECT DISTINCT ledger_sequence FROM transactions WHERE ledger_sequence >= ? AND ledger_sequence <= ? ORDER BY ledger_sequence ASC LIMIT ?` (transaction.go:170-186). The `transactions` table has `CREATE INDEX index_ledger_sequence ON transactions(ledger_sequence)` (02_transactions.sql:10), making this an index-only skip-scan bounded by the LIMIT clause. For typical near-tip queries with small limits, this touches at most ~10-20 index entries and completes in microseconds.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:329-331` — `GetLedgerSequencesWithTransactions` call with `uint32(start.LedgerSequence), lastLedgerSeq, int(limit)+1`
- `cmd/stellar-rpc/internal/db/transaction.go:170-186` — DISTINCT query implementation uses `index_ledger_sequence` B-tree index
- `cmd/stellar-rpc/internal/db/sqlmigrations/02_transactions.sql:10` — index definition confirming `ledger_sequence` is indexed
- `cmd/stellar-rpc/internal/db/transaction.go:73-91` — `InsertTransactions` computes and discards `txCount`, confirming the ingest-side observation is correct
- `cmd/stellar-rpc/internal/methods/get_transactions.go:339-384` — post-planner path: `BatchGetLedgersBySequences`, `UnmarshalBinary`, `processTransactionsInLedger` — these dominate request cost

### Why It Failed

The SQL query being replaced is already negligibly cheap, making the proposed in-memory window unable to produce measurable improvement:

1. **The DISTINCT query is an index-only scan.** With the `index_ledger_sequence` B-tree index, SQLite performs an index skip-scan: it seeks to the start of the range in the index and walks forward collecting distinct values, stopping after `LIMIT` entries. For typical `getTransactions` calls with limits of 1-200, this touches 2-200 index leaf pages — completing in 10-100μs.

2. **Query cost is negligible vs. total request cost.** The remainder of the `getTransactions` path involves: `BatchGetLedgersBySequences` fetching multi-KB LCM blobs from the ledger table, `lcm.UnmarshalBinary` deserializing XDR, `processTransactionsInLedger` walking transactions and building response objects, and JSON encoding via xdr2json FFI. These dominate the request at 10-100ms total. Saving 10-100μs (0.01-1% of request time) is unmeasurable.

3. **Non-trivial implementation complexity for zero payoff.** The in-memory window requires: a new `LedgerBucketWindow` instance tracking non-empty ledger sequences, plumbing from the ingest service through to the RPC handler, a fallback path for queries outside the window, and correct invalidation semantics across backfill/restart. This complexity is unjustified when the SQL query it replaces is already microsecond-fast.

4. **Prior analysis corroborates.** The ingest/002 review already established that "the index query is an index-only scan on SQLite (microseconds), so avoiding it via an in-memory window provides negligible marginal benefit" — the same conclusion applies to this hypothesis's mechanism.

### Lesson Learned

When proposing to replace a SQL query with an in-memory cache, first estimate the query's actual cost. Index-only scans with small LIMIT clauses on SQLite B-tree indexes complete in microseconds — well below the noise floor of typical RPC requests. The threshold for justifying in-memory replacement of an indexed SQL query should be that the query represents at least 5% of total request time, not merely that it exists.
