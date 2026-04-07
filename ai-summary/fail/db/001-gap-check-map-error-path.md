# H001: Missing-ledger gap check allocates a map on the hot path

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Low
**Impact**: CPU / allocation overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

The steady-state `getTransactions` success path should avoid unnecessary per-batch allocations. If gap detection is required, it should only run when the DB is actually missing ledger rows and an error must be returned.

## Mechanism

I initially suspected that the `ledgerMap := make(map[uint32]bool, len(ledgers))` block in `getTransactionsByLedgerSequence()` added avoidable allocation churn to every batch. If that branch were hot, replacing it with an ordered-sequence check would save some work.

## Trigger

Delete one ledger row from the middle of a requested batch so that `len(ledgers) != expectedCount`, then call `getTransactions`. The handler will build the map and walk the range to produce the first missing-ledger error.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:264-279` — missing-ledger detection map

## Evidence

The code does allocate a new `map[uint32]bool` and fill it from the batch before returning the missing-ledger error (`cmd/stellar-rpc/internal/methods/get_transactions.go:268-279`).

## Anti-Evidence

That map allocation only happens inside `if len(ledgers) != expectedCount`, which means the database has already returned a short batch and the request is headed for an error. In the normal contiguous-retention case produced by ingestion, the branch is skipped entirely, so it cannot explain steady-state `getTransactions` latency.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The suspected inefficiency is confined to an exceptional corruption/error path, not the success path the performance objective cares about.

### Lesson Learned

For this endpoint, only allocations that occur on the common successful page-building path are worth keeping as optimization hypotheses; defensive checks that run only when the DB is already inconsistent should be filtered out early.
