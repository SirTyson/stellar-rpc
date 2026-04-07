# H002: Ingest discards per-ledger transaction counts, so hot `getTransactions` pages still plan with a blind 50-ledger batch

**Date**: 2026-04-07
**Subsystem**: ingest
**Severity**: Medium
**Impact**: DB I/O / XDR decode / batch-planning waste
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

For recent ledgers that were just ingested, `getTransactions` should be able to estimate how many ledgers are actually needed to satisfy `limit` before it batch-fetches ledger blobs. If the newest ledger already contains enough transactions for the page, the hot path should fetch only that ledger, not a fixed 50-ledger range.

## Mechanism

`transactionHandler.InsertTransactions()` already computes `txCount := lcm.CountTransactions()` for every ingested ledger, but that per-ledger count is thrown away once the DB insert completes. `getTransactionsByLedgerSequence()` therefore has no cheap recent-ledger cardinality signal and always starts from `const batchSize = 50`, even on dense tip ledgers where the first ledger alone satisfies the whole page. A small ingest-maintained transaction-count window keyed by recent ledger sequence could cap the hot-path `batchEnd` to the minimum recent range needed for the requested `limit`, reducing unnecessary `BatchGetLedgerMetas()` calls and wasted LCM deserialization on dense recent traffic.

## Trigger

1. Ingest a run of dense recent ledgers (for example 50-100 transactions per ledger).
2. Call `getTransactions` near the tip with a small `limit` such as 10.
3. Compare the current fixed-50 batch planner against a version that consults an ingest-populated recent tx-count window before choosing the initial batch end.

## Target Code

- `cmd/stellar-rpc/internal/db/transaction.go:68-80` — ingest computes `txCount` for each ledger, then discards it after write-side use.
- `cmd/stellar-rpc/internal/ingest/service.go:295-320` — per-ledger ingest orchestrates transaction/event insertion but retains no recent per-ledger transaction-count metadata.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:238-264` — request path hardcodes `batchSize := 50` before it knows how many recent ledgers are actually required.
- `cmd/stellar-rpc/internal/ledgerbucketwindow/ledgerbucketwindow.go:9-120` — existing contiguous-ledger ring buffer suitable for recent tx-count retention.
- `cmd/stellar-rpc/internal/feewindow/feewindow.go:37-65` — proven pattern for ingest-fed bounded per-ledger statistics windows.

## Evidence

The write path already computes exactly the recent-density signal that the read path lacks: `InsertTransactions()` reads `CountTransactions()` before doing any inserts, so the system knows the per-ledger transaction count at ingest time essentially for free. Despite that, the handler still has only a fixed `batchSize` constant and no recent-density hint, so it must pessimistically overfetch recent ledgers and discover after the fact that the page was satisfied much earlier. This is distinct from the reviewed DB planner hypotheses because it requires no extra SQL lookup for recent ledgers; the information is already available during ingest.

## Anti-Evidence

This helps only within the size of the recent in-memory window; older or sparse scans still need the DB-driven planners already under review. The implementation must preserve missing-ledger error semantics and avoid trusting stale counts after backfill/reset events.
