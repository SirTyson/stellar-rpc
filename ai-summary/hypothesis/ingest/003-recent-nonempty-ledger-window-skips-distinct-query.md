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
