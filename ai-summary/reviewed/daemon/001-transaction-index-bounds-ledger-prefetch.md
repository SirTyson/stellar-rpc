# H001: Transaction Index Can Bound getTransactions Before 50-Ledger Meta Fetches

**Date**: 2026-04-07
**Subsystem**: daemon
**Severity**: High
**Impact**: redundant DB reads / XDR deserialization
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When a `getTransactions` page asks for only the next `limit` transactions after a cursor, the daemon should deserialize only the ledger metas that can actually contribute those transactions. A small page on a dense ledger range should not force decoding dozens of unrelated `LedgerCloseMeta` blobs before the handler can decide it already has enough rows.

## Mechanism

`fetchLedgerMetas` fetches ledgers in fixed 50-ledger batches via `BatchGetLedgerMetas`, and that DB helper fully unmarshals every `meta` BLOB in the requested range before the handler checks whether `totalTxCount >= limit`. On dense ranges, a single batch can therefore decode and retain 50 full `xdr.LedgerCloseMeta` values even when the first one or two ledgers already satisfy the page. The existing `transactions` lookup table already stores `ledger_sequence` and `application_order`, so the daemon can first query the last ledger needed for the next `limit` transactions and then fetch only that exact ledger span.

## Trigger

Run `getTransactions` with a low limit (for example `1`, `10`, or `50`) against a hot range where recent ledgers contain many transactions. Compare the current path to a version that first issues a bounded transaction-table query like "next `limit` transactions after `(startLedger,startOrder)`" and then calls `BatchGetLedgerMetas` only through the resulting max `ledger_sequence`.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:312-407` — fixed-size `batchSize = 50` loop fetches and retains full ledger metas before the stop check.
- `cmd/stellar-rpc/internal/db/ledger.go:133-155` — `BatchGetLedgerMetas` deserializes every row in the range into `[]xdr.LedgerCloseMeta`.
- `cmd/stellar-rpc/internal/db/sqlmigrations/02_transactions.sql:4-10` — existing transaction lookup table and `ledger_sequence` index that can bound the page without schema work.
- `cmd/stellar-rpc/internal/db/transaction.go:207-214` — proves the codebase already relies on the transaction lookup table for targeted transaction retrieval.

## Evidence

The handler's stop condition is transaction-count based, but it is evaluated only after `readTx.BatchGetLedgerMetas(...)` has already returned a fully decoded slice for the whole 50-ledger range. The SQL migration shows an existing indexed lookup table with exactly the ordering data (`ledger_sequence`, `application_order`) needed to identify how far the next page must scan before touching any ledger meta BLOBs.

## Anti-Evidence

If the ledger range is very sparse, the extra "bound the page first" query may recover less work because several ledgers may still be needed to find `limit` transactions. The win is strongest on dense recent ledgers, where the current 50-ledger batch can overshoot the requested page by an order of magnitude.

---

## Review

**Verdict**: VIABLE
**Severity**: Medium
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced `getTransactionsByLedgerSequence` → `fetchLedgerMetas` → `BatchGetLedgerMetas` end-to-end. Confirmed that `fetchLedgerMetas` (lines 312-408) uses a hardcoded `batchSize = 50` and calls `readTx.BatchGetLedgerMetas` which issues a single `SELECT meta FROM ledger_close_meta WHERE sequence >= ? AND sequence <= ? ORDER BY sequence ASC` and fully deserializes every row into `[]xdr.LedgerCloseMeta` via the Go XDR scanner before returning. The transaction-count check (`totalTxCount >= limit`) at lines 399-403 fires only after the entire batch is decoded. With a default limit of 50 and typical mainnet ledger densities of 50-200+ txns/ledger, 48-49 out of 50 decoded LedgerCloseMeta values are routinely discarded.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:350-405` — `fetchLedgerMetas`: hardcoded `batchSize = 50`, calls `BatchGetLedgerMetas` for the full batch, then counts transactions post-decode. No per-ledger early-exit within a batch.
- `cmd/stellar-rpc/internal/db/ledger.go:135-155` — `BatchGetLedgerMetas`: selects all `meta` BLOBs in `[start, end]` range, deserializes all rows into `[]xdr.LedgerCloseMeta` via `l.tx.Select(ctx, &results, query)`. No streaming or lazy decode.
- `cmd/stellar-rpc/internal/db/sqlmigrations/02_transactions.sql:4-10` — `transactions` table schema: `hash BLOB PRIMARY KEY, ledger_sequence INTEGER NOT NULL, application_order INTEGER NOT NULL` with `CREATE INDEX index_ledger_sequence ON transactions(ledger_sequence)`.
- `cmd/stellar-rpc/internal/db/transaction.go:97-142` — `InsertTransactions`: fee-bump txns insert two rows (inner + outer hash) with the same `(ledger_sequence, application_order)`, so `SELECT DISTINCT ledger_sequence, application_order` correctly deduplicates.
- `cmd/stellar-rpc/internal/config/options.go:362-369` — default limit is 50, max is 200.

### Findings

**The inefficiency is real and on a hot path.** Every `getTransactions` request executes `fetchLedgerMetas`, which unconditionally decodes a 50-ledger batch of LedgerCloseMeta XDR. Each LedgerCloseMeta for a dense ledger can be hundreds of KB to several MB. With default limit=50 and typical mainnet densities (50-200+ txns/ledger), only 1-2 ledgers are needed but 50 are decoded. This represents ~96-98% wasted CPU in the XDR deserialization phase.

**The fix is architecturally sound.** Two viable approaches exist:

1. **Transactions-table pre-query** (as proposed): Query `SELECT MAX(ledger_sequence) FROM (SELECT DISTINCT ledger_sequence, application_order FROM transactions WHERE ledger_sequence >= ? ORDER BY ledger_sequence, application_order LIMIT ?)` to find the upper bound, then call `BatchGetLedgerMetas` with the narrowed range. This uses the existing `index_ledger_sequence` for an efficient index scan.

2. **Streaming cursor approach** (simpler alternative): Replace `BatchGetLedgerMetas` with a cursor-based SQL query that decodes one ledger at a time, counting transactions after each decode and stopping when `totalTxCount >= limit`. This pattern already exists in `StreamLedgerRange` (ledger.go:208-234) and avoids the cross-table dependency. The single SQL query still benefits from sequential I/O, but only materializes needed rows.

**No correctness concerns.** The decoded `LedgerCloseMeta` values are identical regardless of batch size. The transactions table and ledger_close_meta table are in the same SQLite database, and the read transaction (`ledgerReaderTx.tx` is a `db.SessionInterface` with an active transaction) can query both tables consistently.

**Severity rationale**: Rated Medium rather than High because Phase 2 (transaction processing, XDR marshaling, JSON FFI) is also a significant cost component of the total request latency. The deserialization savings are substantial in absolute terms but represent a fraction of end-to-end time. For extreme cases (limit=1 on very dense ledgers), the improvement could approach High territory.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/methods/get_transactions.go:fetchLedgerMetas` (lines 312-408) and `cmd/stellar-rpc/internal/db/ledger.go` (add streaming method to `ledgerReaderTx`)
- **Change description (preferred approach — streaming cursor)**: Add a `StreamLedgerMetas(ctx, start, end uint32, fn func(xdr.LedgerCloseMeta) (bool, error)) error` method to `ledgerReaderTx` that uses `l.tx.Query()` + per-row `Scan(&lcm)` with an early-termination callback. Modify `fetchLedgerMetas` to use this method, accumulating metas and counting transactions per-ledger, breaking when `totalTxCount >= limit`. The `LedgerReaderTx` interface must be extended accordingly.
- **Change description (alternative — transactions-table pre-query)**: Add `GetMaxLedgerForLimit(ctx, startLedger uint32, startOrder int, limit uint) (uint32, error)` to `ledgerReaderTx` that queries the transactions table to find the upper-bound ledger. Modify `fetchLedgerMetas` to compute `batchEnd = min(preQueryResult, batchStart + batchSize - 1)`.
- **Correctness check**: Existing tests in `cmd/stellar-rpc/internal/methods/get_transactions_test.go` (multiple scenarios with limit=10, maxLimit=100) cover the handler. Integration tests in `cmd/stellar-rpc/internal/integrationtest/` cover end-to-end. Verify the gap-detection logic (lines 380-395) still works with smaller batches.
- **Benchmark focus**: Measure `fetchLedgerMetas` latency and total `getTransactions` latency with limit=10 on a range where each ledger has 100+ transactions. The streaming approach should show ~10-20x fewer LedgerCloseMeta decodes and a corresponding reduction in Phase 1 CPU time. Memory allocation should also drop significantly (fewer large XDR structs retained).
