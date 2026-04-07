# H004: Sorted Non-Empty Ledger Sequences Are Never Collapsed Back Into Contiguous Range Reads

**Date**: 2026-04-07
**Subsystem**: methods
**Severity**: Low
**Impact**: latency / SQL overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When the transaction index returns long contiguous runs of non-empty ledgers, `getTransactions` should reuse the existing range-read helpers for those runs instead of always rebuilding the fetch as a by-sequence `IN (...)` query. Exact ledger selection should be preserved, but contiguous spans should ride the cheaper sequential path.

## Mechanism

`GetLedgerSequencesWithTransactions()` returns sequences ordered ascending, and active near-tip traffic often produces many consecutive non-empty ledgers. `getTransactionsByLedgerSequence()` currently hands that sorted slice straight to `BatchGetLedgersBySequences()`, which always emits `WHERE sequence IN (...) ORDER BY sequence ASC` even when the input is effectively one or two contiguous ranges. Collapsing `ledgerSeqs` into `[start,end]` runs and fetching each run with `BatchGetLedgers()` (or a tx-scoped streaming range helper) should reduce SQL construction and let SQLite walk adjacent ledger rows sequentially without changing the set of ledgers returned.

## Trigger

1. Benchmark `getTransactions` on dense recent traffic where the planner returns many adjacent ledger sequences.
2. Capture query plans and latency for the current by-sequence fetch versus a run-collapsing variant that converts adjacent sequences into range reads.
3. Focus on pages where the selected sequence list is mostly contiguous but still produced through the transaction-index path.

## Target Code

- `cmd/stellar-rpc/internal/db/transaction.go:170-186` — the planner query returns ledger sequences in ascending order.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:304-315` — the sorted sequence slice is forwarded unchanged to the by-sequence fetch helper.
- `cmd/stellar-rpc/internal/db/ledger.go:93-136` — `BatchGetLedgers()` already provides an ordered contiguous-range fetch path.
- `cmd/stellar-rpc/internal/db/ledger.go:162-204` — `BatchGetLedgersBySequences()` always builds the fetch as an `IN (...)` query.

## Evidence

The current code already has both ingredients needed for run coalescing: the planner emits sorted sequence numbers, and the DB layer has a dedicated contiguous-range helper. That makes the unconditional `IN (...)` fetch look like a missed normalization step in the handoff between planner and fetcher, especially on active ledgers where non-empty sequences cluster tightly.

## Anti-Evidence

This does little for truly sparse histories where the selected sequences are isolated singletons, and exact planner improvements are likely to deliver larger wins on dense pages. The benefit here is therefore a smaller, workload-dependent reduction in SQL overhead rather than a universal hot-path fix.
