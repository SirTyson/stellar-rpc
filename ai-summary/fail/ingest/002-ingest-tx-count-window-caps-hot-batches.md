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

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated (different mechanism from db/H001 index-driven planner, but targets the same problem)
**Failed At**: reviewer

### Trace Summary

Traced the `getTransactionsByLedgerSequence` batch loop (get_transactions.go:247-301) and confirmed the `batchEnd` cap at line 259-261 (`if batchEnd > lastLedgerSeq { batchEnd = lastLedgerSeq }`). For the hypothesis's primary scenario — near-tip queries with small limits — the batch is naturally capped to the distance between `startLedger` and `lastLedgerSeq`, not to the full 50. The scenarios where over-fetching IS significant (catch-up queries starting far behind the tip) fall outside the proposed in-memory window's coverage. The already-reviewed db/H001 (application-order-page-planner) provides a strictly superior index-driven solution covering all query ranges.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:258-261` — `batchEnd` is capped to `lastLedgerSeq`, so near-tip queries naturally fetch only 1-5 ledgers, not 50
- `cmd/stellar-rpc/internal/methods/get_transactions.go:241-247` — `batchSize=50` is a ceiling, not a floor; actual batch width depends on distance to tip
- `cmd/stellar-rpc/internal/db/transaction.go:68-80` — `InsertTransactions` does compute and discard `txCount`, confirming the ingest-side observation
- `cmd/stellar-rpc/internal/db/ledger.go:120-140` — `BatchGetLedgerMetas` fetches all LCMs in the capped range
- `ai-summary/reviewed/db/001-application-order-page-planner.md` — already-reviewed VIABLE/High hypothesis that uses the transactions index to determine exact page boundaries for ALL query ranges, not just recent ones

### Why It Failed

Three independent reasons prevent this hypothesis from being viable:

1. **The near-tip scenario is already efficient.** The hypothesis's primary claimed benefit — avoiding a 50-LCM fetch when querying near the tip — doesn't match the code. `batchEnd` is capped to `lastLedgerSeq` (line 259-261), so a query starting at `tip - 3` with the tip as `lastLedgerSeq` fetches only 4 ledgers, not 50. The "blind 50-ledger batch" only occurs when `startLedger` is 50+ ledgers behind the tip.

2. **The in-memory window covers exactly the wrong range.** Catch-up queries starting 50+ ledgers behind the tip are where over-fetching is worst, but these queries are likely to start outside the in-memory window (which would hold only the most recent N ledgers). The optimization helps most where the window has data (near the tip), but the tip is exactly where the batch cap already limits waste.

3. **Subsumed by a strictly superior approach.** The reviewed db/H001 (application-order-page-planner) queries the `transactions` table index to determine exact page boundaries — `SELECT DISTINCT ledger_sequence FROM transactions WHERE ledger_sequence >= ? ORDER BY ledger_sequence, application_order LIMIT ?`. This provides exact (not estimated) boundaries for ALL query ranges, not just those within the recent window. The index query is an index-only scan on SQLite (microseconds), so avoiding it via an in-memory window provides negligible marginal benefit.

### Lesson Learned

When evaluating batch-planning optimizations, check whether the batch size is actually applied as claimed. The `batchSize=50` constant is a maximum stride, not a minimum — the actual batch width is `min(batchSize, lastLedgerSeq - batchStart + 1)`. Near-tip queries naturally have small batches because there simply aren't 50 future ledgers to fetch. Ingest-side metadata windows are most valuable when they provide information unavailable elsewhere (like FeeWindow's per-ledger fee percentiles); when the same information is available more completely via a cheap index query, the index approach is superior.
