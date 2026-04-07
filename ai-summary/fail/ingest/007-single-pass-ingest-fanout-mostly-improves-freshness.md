# H007: Collapsing ingest's three transaction walks mostly improves ledger freshness, not `getTransactions` throughput

**Date**: 2026-04-07
**Subsystem**: ingest
**Severity**: Low
**Impact**: ingest CPU / post-close availability
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

An optimization for this pass should reduce `getTransactions` request latency or throughput cost on the endpoint itself. Reworking ingest-only CPU should count only if the code strongly suggests that requests are directly blocked on that work for a meaningful portion of their execution time.

## Mechanism

I investigated whether `ingestLedgerCloseMeta()` and fee-window ingestion should share a single ledger transaction traversal, because the current code opens a fresh `ingest.NewLedgerTransactionReaderFromLedgerCloseMeta()` in `InsertTransactions()`, `InsertEvents()`, and `FeeWindows.IngestFees()`. The duplicate work is real, but the direct payoff is a shorter "ledger close to committed visibility" delay for the newest ledger, not a lower steady-state CPU cost inside the `getTransactions` handler once a request is actually processing its ledger set.

## Trigger

1. Run tip-following ingest on dense ledgers that have transactions, events, and fee-window work.
2. Poll `getTransactions` for the newest ledger immediately after each close.
3. Compare the current three-pass ingest path against a version that fans transaction indexing, event insertion, and fee-window extraction out from one shared ledger traversal.

## Target Code

- `cmd/stellar-rpc/internal/ingest/service.go:ingest:190-242` — latest-ledger visibility is published only after all ingest-side work and commit complete.
- `cmd/stellar-rpc/internal/ingest/service.go:ingestLedgerCloseMeta:295-320` — sequentially calls the ledger, transaction, and event writers.
- `cmd/stellar-rpc/internal/db/transaction.go:InsertTransactions:73-148` — first full ledger transaction reader pass.
- `cmd/stellar-rpc/internal/db/event.go:InsertEvents:76-247` — second full ledger transaction reader pass.
- `cmd/stellar-rpc/internal/feewindow/feewindow.go:IngestFees:163-232` — third full ledger transaction reader pass.

## Evidence

All three code paths independently construct an SDK `LedgerTransactionReader`, which in turn builds its own `envelopesByHash` map and iterates the ledger in processing order. That means the same ledger is hashed and walked repeatedly before `writeTx.Commit()` publishes the new latest ledger to readers.

## Anti-Evidence

The `getTransactions` request path still pays the same `GetLedgerRange`, planner query, LCM decode, envelope hashing, `ParseTransaction`, and JSON/XDR encoding costs after commit, regardless of how ingest performed its internal fanout. The main effect of the ingest rewrite would be making the newest ledger queryable sooner after close, which is freshness-oriented and only indirectly related to request latency.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

This path optimizes ingest's pre-commit work more than it optimizes `getTransactions` execution itself. The likely improvement is shorter close-to-visibility delay for newest-ledger polling, not a measurable reduction in steady-state `getTransactions` handler latency or RPS on the endpoint hot path.

### Lesson Learned

Repeated ingest-side work is only in scope for this objective when requests actually wait on that work during normal execution. If the gain is mainly "the new ledger becomes visible a bit sooner," treat it as freshness or pipeline efficiency unless the code shows a direct request-time stall.
