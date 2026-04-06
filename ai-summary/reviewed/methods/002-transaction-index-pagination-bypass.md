# H002: Pagination Ignores the Existing Transaction Index and Walks Empty Ledgers

**Date**: 2026-04-06
**Subsystem**: methods
**Severity**: High
**Impact**: latency / CPU / DB I/O
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

For cursor-based pagination, `getTransactions` should use the existing transaction index to identify the next `limit` transaction positions after the cursor, then load only the ledgers that actually contain those transactions. A request should not linearly scan every retained ledger just to discover that most of them contribute nothing to the response.

## Mechanism

The handler currently derives the next page by incrementing a TOID cursor and walking ledger-by-ledger until it accumulates enough transactions. But the repository already maintains a `transactions` index with `ledger_sequence` and `application_order`, so the next page could be found with an ordered SQL query over distinct `(ledger_sequence, application_order)` pairs and then hydrated from only the touched ledgers. On sparse windows this should avoid scanning empty ledgers entirely, which is a larger algorithmic win than merely batching the current per-ledger fetches.

## Trigger

1. Backfill a window where many consecutive ledgers are empty or contain only one transaction.
2. Request `getTransactions` from an old `startLedger` or from a cursor just before a long sparse region.
3. Measure latency and ledgers scanned by the current linear walk versus a prototype that pages from the `transactions` table and then loads only referenced ledgers.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:248-276` — pagination is implemented as a linear ledger scan.
- `cmd/stellar-rpc/internal/db/transaction.go:97-121` — ingestion already records `ledger_sequence` and `application_order` for each transaction.
- `cmd/stellar-rpc/internal/db/sqlmigrations/02_transactions.sql:4-10` — the `transactions` table and `ledger_sequence` index already exist.
- `cmd/stellar-rpc/internal/db/transaction.go:194-235` — `GetTransaction` demonstrates using `application_order` to jump directly to a transaction within a ledger.

## Evidence

The codebase already pays ingestion cost to maintain a transaction-position index, but `getTransactions` does not consult it at all. That leaves the endpoint doing algorithmic work proportional to the number of ledgers scanned instead of the number of transactions returned.

## Anti-Evidence

Fee-bump transactions are indexed by both outer and inner hash, so a paging query would need `DISTINCT` or grouping on `(ledger_sequence, application_order)` to avoid double-counting. The response still needs full ledger metadata for hydration, so this does not eliminate ledger decoding entirely; it only narrows the set of ledgers that must be touched.

---

## Review

**Verdict**: VIABLE
**Severity**: Medium
**Date**: 2026-04-06
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

The `getTransactionsByLedgerSequence` loop (get_transactions.go:253-276) iterates every ledger from the cursor to the end of the retention window, fetching and fully deserializing each ledger's `LedgerCloseMeta` via `fetchLedgerData` → `getLedgerFromDB` (ledger.go:326-341), regardless of whether the ledger contains any transactions. The `transactions` table (schema: `hash BLOB PK, ledger_sequence INTEGER, application_order INTEGER` with an index on `ledger_sequence`) already tracks exactly which ledgers contain transactions and at which positions. A query on this table would identify only non-empty ledgers, eliminating all wasted LCM fetches and XDR deserializations for empty ledgers. This is algorithmically distinct from H001 (which batches reads but still touches empty ledgers).

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:253-276` — Main loop iterates `for ledgerSeq := start.LedgerSequence; ledgerSeq <= int32(ledgerRange.LastLedger.Sequence)`, calling `fetchLedgerData` for every ledger regardless of transaction content.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:71-88` — `fetchLedgerData` delegates to `readTx.GetLedger(ctx, ledgerSeq)`, a point query that fetches and fully deserializes the LCM blob even for empty ledgers.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:94-213` — `processTransactionsInLedger` calls `ledger.CountTransactions()` and loops `for i := startTxIdx; i <= txCount`. For empty ledgers (txCount=0), the loop body never executes, but the LCM has already been fetched and deserialized — the waste occurs in `fetchLedgerData`, not here.
- `cmd/stellar-rpc/internal/db/ledger.go:326-341` — `getLedgerFromDB` builds a squirrel query, calls `db.Select`, allocates `[]xdr.LedgerCloseMeta`, deserializes the full XDR blob. This is the per-ledger cost that H002 would eliminate for empty ledgers.
- `cmd/stellar-rpc/internal/db/transaction.go:97-121` — `InsertTransactions` stores `(hash, ledger_sequence, application_order)` for every transaction during ingestion. Fee-bump transactions store two entries (inner hash and outer hash) with the same `(ledger_sequence, application_order)`, so a `DISTINCT` on those columns correctly deduplicates.
- `cmd/stellar-rpc/internal/db/sqlmigrations/02_transactions.sql:4-10` — The `transactions` table has `CREATE INDEX index_ledger_sequence ON transactions(ledger_sequence)`, which supports efficient range queries like `WHERE ledger_sequence >= ? ORDER BY ledger_sequence`.
- `cmd/stellar-rpc/internal/db/transaction.go:194-235` — `getTransactionByHash` demonstrates the join pattern: querying the `transactions` table to find `(ledger_sequence, application_order)`, then joining with `ledger_close_meta` to hydrate only the needed LCM. This same pattern could be adapted for pagination.

### Findings

1. **The optimization is algorithmically real and distinct from H001.** H001 proposes batching ledger reads (reducing SQL round-trips from N to N/chunk). H002 proposes skipping empty ledgers entirely (reducing ledger fetches from N_ledgers to N_nonempty_ledgers). These are complementary: H001 reduces constant-factor overhead per ledger; H002 reduces the number of ledgers touched. On sparse workloads, H002 provides a larger win.

2. **The required index already exists.** The `index_ledger_sequence` index on the `transactions` table supports efficient queries like `SELECT DISTINCT ledger_sequence FROM transactions WHERE ledger_sequence >= ? ORDER BY ledger_sequence LIMIT ?`. No schema change is needed.

3. **Fee-bump deduplication is manageable.** Fee-bump transactions create two rows with the same `(ledger_sequence, application_order)`. A `SELECT DISTINCT ledger_sequence, application_order` query correctly deduplicates. For the simpler "which ledgers have transactions" query, `SELECT DISTINCT ledger_sequence` is unaffected since both entries share the same ledger.

4. **Cursor translation is straightforward.** The current TOID cursor encodes `(LedgerSequence, TransactionOrder, OperationOrder)`. The `transactions` table stores `(ledger_sequence, application_order)`. `application_order` maps directly to `TransactionOrder` in the TOID. A cursor-based SQL query would be: `WHERE (ledger_sequence > ?) OR (ledger_sequence = ? AND application_order > ?) ORDER BY ledger_sequence, application_order LIMIT ?`.

5. **Severity: Medium, not High.** On sparse workloads (e.g., testnets where 90%+ of ledgers are empty), this could eliminate 90% of LCM fetches and deserializations — potentially >20% latency reduction. However, on busy mainnet workloads where most ledgers contain transactions, the improvement is minimal since nearly every ledger would be touched anyway. Averaged across typical usage patterns, Medium (5-20%) is the appropriate rating. The optimization's value is workload-dependent and inversely proportional to network transaction density.

6. **The full LCM is still needed for hydration.** Even with the index-based approach, each identified non-empty ledger must have its LCM fetched and deserialized to extract transaction data (result, meta, envelope, events). H002 eliminates waste for empty ledgers but does not reduce per-transaction processing cost.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/db/transaction.go` (add a new `GetTransactionPositions` or `PageTransactions` method) and `cmd/stellar-rpc/internal/methods/get_transactions.go` (replace the linear ledger loop with an index-driven fetch).
- **Change description**: Add a DB method that queries the `transactions` table for the next N distinct `(ledger_sequence, application_order)` pairs after a given cursor position. Use this to identify the set of non-empty ledgers, then fetch only those ledgers' LCMs (potentially using `BatchGetLedgers` from H001 if available, or individual fetches). Rewrite `getTransactionsByLedgerSequence` to use this index-first-then-hydrate pattern instead of the linear scan.
- **Correctness check**: Existing tests in `cmd/stellar-rpc/internal/methods/get_transactions_test.go` cover pagination, cursor handling, and ledger boundary conditions. Verify fee-bump transaction pagination produces identical results by comparing output of the old and new implementations on the same test data. The `TransactionReader` mock will need a new method for the index query.
- **Benchmark focus**: Measure `getTransactions` latency on a database with 1000+ ledgers where <10% contain transactions, using `limit=200` and `startLedger` near the oldest retained ledger. Primary metric: number of LCM fetches (should drop from ~2000 to ~200). Secondary metric: end-to-end request latency, targeting >50% reduction on sparse workloads. Also benchmark on a dense workload to confirm no regression.
