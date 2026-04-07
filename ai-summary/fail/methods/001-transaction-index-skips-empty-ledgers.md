# H001: `getTransactions` Ignores the Ingested Transaction Index and Still Decodes Empty Ledgers

**Date**: 2026-04-07
**Subsystem**: methods
**Severity**: High
**Impact**: latency / DB I/O / allocations
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When `getTransactions` scans a sparse window, it should use the already-ingested transaction index to jump directly to ledgers that actually contain transactions. A page that needs 50-200 transactions from a mostly empty retention window should not deserialize every intervening `LedgerCloseMeta` blob just to learn that most ledgers are empty.

## Mechanism

`getTransactionsByLedgerSequence` currently walks contiguous ledger batches and feeds every returned `LedgerCloseMeta` into `processTransactionsInLedger`, even when `CountTransactions()` is zero. But ingestion already writes one row per transaction into the `transactions` table, keeps that table trimmed in the same retention window, and maintains an index on `ledger_sequence`. A tx-scoped query like `SELECT DISTINCT ledger_sequence FROM transactions WHERE ledger_sequence >= ? AND ledger_sequence <= ? ORDER BY ledger_sequence ASC LIMIT ?` could identify the next non-empty ledgers inside the same snapshot, letting the handler fetch only those metas instead of decoding empty-ledger blobs.

## Trigger

1. Populate a retention window where transactions appear only every N ledgers (for example every 10th-20th ledger).
2. Call `getTransactions` from an older `startLedger` with `limit=50` or `limit=200`.
3. Compare latency and bytes allocated against a version that first queries distinct `transactions.ledger_sequence` values and only fetches metadata for those ledgers.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:238-301` — the handler still fetches contiguous ledger-meta batches and processes every ledger in them.
- `cmd/stellar-rpc/internal/db/transaction.go:68-142` — ingestion already materializes per-transaction rows keyed by `ledger_sequence` and `application_order`.
- `cmd/stellar-rpc/internal/db/transaction.go:150-162` — transaction rows are trimmed in lockstep with ledger retention, so the table is a safe sparse index for the current window.
- `cmd/stellar-rpc/internal/db/sqlmigrations/02_transactions.sql:4-10` — the `transactions` table already has an index on `ledger_sequence`.

## Evidence

The live hot path has no transaction-aware planning step: it goes straight from `GetLedgerRange` to `BatchGetLedgerMetas`, so a sparse request still deserializes every ledger between `batchStart` and `batchEnd`. The DB layer already pays the cost to build and maintain a much smaller transaction index during ingestion, which means the information needed to skip empty ledgers is present but unused.

## Anti-Evidence

Dense ledgers benefit less because most ledgers in the range would still need to be fetched. Fee-bump transactions also create duplicate hash rows in `transactions`, so the planner must use `DISTINCT ledger_sequence` (or deduplicate by `application_order`) rather than raw row counts.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: FAIL — duplicate of reviewed/db/002-sparse-scan-ignores-transaction-index.md
**Failed At**: reviewer

### Trace Summary

This hypothesis proposes using `SELECT DISTINCT ledger_sequence FROM transactions` to skip empty ledgers in `getTransactions`. The identical optimization — same mechanism, same target code paths, same SQL query shape — has already been investigated and marked VIABLE as db/002-sparse-scan-ignores-transaction-index. Additionally, db/001-application-order-page-planner extends this further with row-level precision, subsumes it, and is also marked VIABLE. Both are already in the review pipeline.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:238-301` — same batch loop identified in db/002
- `cmd/stellar-rpc/internal/db/transaction.go:68-142` — same ingestion path cited in db/002
- `cmd/stellar-rpc/internal/db/sqlmigrations/02_transactions.sql:4-10` — same index cited in db/002

### Why It Failed

This is a duplicate of `ai-summary/reviewed/db/002-sparse-scan-ignores-transaction-index.md`, which proposes the identical optimization: use `SELECT DISTINCT ledger_sequence FROM transactions WHERE ledger_sequence >= ? ORDER BY ledger_sequence LIMIT ?` to skip empty ledgers in `getTransactions`. The target code, mechanism, proposed SQL query, and expected impact are all the same. The db/002 hypothesis has already been reviewed as VIABLE/High and includes complete PoC guidance. Furthermore, db/001-application-order-page-planner provides a superset approach with row-level precision that also covers the sparse-ledger case.

### Lesson Learned

Cross-subsystem deduplication is important: the same optimization can be framed from either the methods (caller) or db (data layer) perspective. The db subsystem is the more natural home since the fix requires adding a new query method to `TransactionReader`.
