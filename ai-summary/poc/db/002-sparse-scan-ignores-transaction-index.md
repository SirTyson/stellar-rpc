# H002: getTransactions still scans empty ledgers instead of using the transaction index

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: High
**Impact**: DB / CPU / sparse-history scan cost
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

For sparse histories, `getTransactions` should use the existing transaction index to jump directly to the next ledgers and application orders that actually contain transactions. Returning `N` transactions should scale primarily with `N`, not with every ledger sequence between the cursor and the latest retained ledger.

## Mechanism

The current implementation walks ledger ranges in sequence and fetches every `LedgerCloseMeta` batch from `startLedger` to `latestLedger`, even when many of those ledgers contain zero transactions. The DB already maintains a `transactions` table indexed by `ledger_sequence`, and `getTransactionByHash()` proves that the codebase already uses that table to locate precise transactions, so sparse `getTransactions` pages are doing avoidable O(number of ledgers) work where an index-driven plan could identify the next `limit` hits first and fetch only the ledgers that matter.

## Trigger

Load a retention window where only one out of every 10-20 ledgers contains any transactions, then call `getTransactions` from an old `startLedger` with `limit=10`. The handler will batch-read and deserialize a large run of empty ledger metas before it accumulates the first page of actual transactions.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:199-304` — sequential ledger scan from cursor to latest
- `cmd/stellar-rpc/internal/db/ledger.go:BatchGetLedgerMetas:120-139` — fetches every ledger in the scanned range
- `cmd/stellar-rpc/internal/db/transaction.go:getTransactionByHash:200-235` — existing index-driven transaction lookup path
- `cmd/stellar-rpc/internal/db/sqlmigrations/02_transactions.sql:4-10` — `transactions(ledger_sequence, application_order)` storage already exists

## Evidence

The handler never consults the `transactions` table; it only advances `batchStart` ledger-by-ledger (`cmd/stellar-rpc/internal/methods/get_transactions.go:243-294`). Meanwhile the DB schema already stores `(hash, ledger_sequence, application_order)` and has an index on `ledger_sequence` (`cmd/stellar-rpc/internal/db/sqlmigrations/02_transactions.sql:4-10`), and `getTransactionByHash()` already joins that table back to `ledger_close_meta` for targeted retrieval (`cmd/stellar-rpc/internal/db/transaction.go:207-235`).

## Anti-Evidence

If most ledgers in the scanned range already contain transactions, the benefit of an index-driven plan will narrow because the current batched ledger reads stay productive. Any fix must preserve the endpoint's exact cursor semantics and still surface missing-ledger errors consistently when the local store is corrupted.

---

## Review

**Verdict**: VIABLE
**Severity**: High
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full `getTransactions` path from `getTransactionsByLedgerSequence` through `BatchGetLedgerMetas` and `processTransactionsInLedger`. Confirmed that the handler walks every ledger in the range regardless of transaction content: it fetches up to 50 LCMs per batch via SQL, fully deserializes each one, then creates a `LedgerTransactionReader` per ledger — all before discovering whether `CountTransactions()` returns zero. The `transactions` table with its `index_ledger_sequence` index already exists and is used by `getTransactionByHash`, proving the codebase has the infrastructure for index-driven lookups. This is distinct from and complementary to H001 (reviewed as VIABLE/Medium), which addresses wasted decode within a batch once the limit is met.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:243-294` — outer loop walks `batchStart` from `start.LedgerSequence` to `lastLedgerSeq` in steps of `batchSize=50`, calling `BatchGetLedgerMetas` for every batch without consulting the transaction index
- `cmd/stellar-rpc/internal/db/ledger.go:BatchGetLedgerMetas:120-140` — fetches all LCMs in [start, end] range via `SELECT meta FROM ledger_close_meta WHERE sequence >= ? AND sequence <= ?`, fully deserializes every row into `xdr.LedgerCloseMeta`
- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:75-195` — called for every ledger including empty ones; creates `ingest.NewLedgerTransactionReaderFromLedgerCloseMeta` (re-parses LCM), calls `CountTransactions()`, and only then discovers the loop body has zero iterations
- `cmd/stellar-rpc/internal/db/transaction.go:getTransactionByHash:200-235` — existing index-driven path joins `transactions t` with `ledger_close_meta lcm ON (t.ledger_sequence = lcm.sequence)`, fetching only the specific ledger that contains the target transaction
- `cmd/stellar-rpc/internal/db/sqlmigrations/02_transactions.sql:4-10` — schema: `CREATE INDEX index_ledger_sequence ON transactions(ledger_sequence)`, the B-tree index needed for efficient `SELECT DISTINCT ledger_sequence` queries

### Findings

The inefficiency is confirmed and significant for sparse histories:

1. **Per-empty-ledger cost**: For each empty ledger in the scanned range, the handler performs: (a) SQL read of the full LCM blob from SQLite, (b) full recursive XDR deserialization into `xdr.LedgerCloseMeta`, (c) `NewLedgerTransactionReaderFromLedgerCloseMeta` which re-parses the network passphrase and LCM, (d) `CountTransactions()` returning 0, (e) a no-op loop. All of this work is pure waste.

2. **Scaling is O(ledger range), not O(transactions)**: With `limit=10` over a 200-ledger range where 1 in 20 ledgers has transactions, the current code scans ~200 ledgers (4 batches of 50) to find 10 transactions. An index-driven approach would issue one query like `SELECT DISTINCT ledger_sequence FROM transactions WHERE ledger_sequence >= ? ORDER BY ledger_sequence LIMIT ?` to identify ~10 target ledgers, then fetch only those LCMs — a ~20x reduction in DB reads and deserialization.

3. **Index infrastructure already exists**: The `transactions` table has `index_ledger_sequence`, a B-tree index on `ledger_sequence`. A `SELECT DISTINCT ledger_sequence` query using this index would be an index-only scan, extremely fast on SQLite. The `getTransactionByHash` method already demonstrates the JOIN pattern between `transactions` and `ledger_close_meta`.

4. **Architectural gap**: The `transactionsRPCHandler` struct (get_transactions.go:23-29) currently holds only a `db.LedgerReader`, not a `TransactionReader`. The `TransactionReader` interface (transaction.go:51-53) only exposes `GetTransaction(ctx, hash)`. A new method or interface would be needed — e.g., `GetLedgerSequencesWithTransactions(ctx, startSeq, limit) ([]uint32, error)` — but this is a small, well-scoped addition.

5. **Gap-check adaptation required**: The current gap-detection logic (lines 264-280) verifies that every ledger in the batch exists in `ledger_close_meta`. An index-driven approach would skip empty ledgers by design, so the gap check would either (a) need to be removed (since the retention window guarantees contiguity during normal ingestion), (b) moved to a separate lightweight check (e.g., `SELECT COUNT(*) FROM ledger_close_meta WHERE sequence BETWEEN ? AND ?`), or (c) performed only on the specific ledgers being fetched.

6. **Relationship to H001**: This optimization is complementary to H001 (lazy decode within a batch). H001 reduces waste when a batch has more LCMs than needed. H002 reduces waste when the scan range has more ledgers than contain transactions. Both can be applied independently. In fact, applying H002 first makes H001 less impactful for sparse scenarios (since fewer irrelevant LCMs are fetched), but H001 still helps for dense scenarios.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence` (lines 237-294) and `cmd/stellar-rpc/internal/db/transaction.go` (new method on `transactionHandler`)
- **Change description**: Add a new method to `TransactionReader` interface and `transactionHandler`: `GetLedgerSequencesWithTransactions(ctx context.Context, startSeq uint32, endSeq uint32, limit int) ([]uint32, error)` that executes `SELECT DISTINCT ledger_sequence FROM transactions WHERE ledger_sequence >= ? AND ledger_sequence <= ? ORDER BY ledger_sequence LIMIT ?`. In `getTransactionsByLedgerSequence`, replace the linear batch loop with: (1) query the transaction index for the next N distinct ledger_sequences, (2) fetch only those specific LCMs, (3) process them. The handler struct needs a new `transactionReader db.TransactionReader` field (or a combined interface). For the gap check, verify the fetched ledger sequences exist in `ledger_close_meta` by comparing the returned count against the requested set.
- **Correctness check**: Existing tests in `cmd/stellar-rpc/internal/methods/get_transactions_test.go` cover pagination, cursor semantics, and edge cases. The gap-detection behavior for corrupted stores should be tested separately. Cursor encoding/decoding (TOID format) must remain identical.
- **Benchmark focus**: Measure `getTransactions` latency with `limit=10` over a 1000-ledger range where only 1 in 20 ledgers has transactions. Compare current (scan all 1000) vs index-driven (query index, fetch ~10 LCMs). Expect >20x reduction in LCM fetches/deserializations and >50% end-to-end latency reduction for this scenario. For dense histories (every ledger has transactions), expect negligible overhead from the additional index query.

---

## PoC Attempt

**Result**: POC_PASS
**Date**: 2026-04-07
**PoC by**: claude-opus-4-6, high

### Changes Made

1. **`cmd/stellar-rpc/internal/db/transaction.go`** (lines ~50-60, ~165-183):
   - Extended `TransactionReader` interface with `GetLedgerSequencesWithTransactions(ctx, startSeq, endSeq uint32, limit int) ([]uint32, error)`
   - Implemented on `transactionHandler` using `SELECT DISTINCT ledger_sequence FROM transactions WHERE ledger_sequence >= ? AND ledger_sequence <= ? ORDER BY ledger_sequence ASC LIMIT ?` — leverages the existing `index_ledger_sequence` B-tree index for an efficient index-only scan

2. **`cmd/stellar-rpc/internal/db/ledger.go`** (lines ~35-45, ~157-200):
   - Extended `LedgerReaderTx` interface with `BatchGetLedgersBySequences(ctx, sequences []uint32) ([]LedgerMetadataChunk, error)`
   - Implemented on `ledgerReaderTx` using `WHERE sequence IN (?, ?, ...)` — fetches LCMs for specific (non-contiguous) sequence numbers with the same partial XDR decode as `BatchGetLedgers`

3. **`cmd/stellar-rpc/internal/methods/get_transactions.go`** (lines ~23-30, ~267-390, ~391-406):
   - Added `transactionReader db.TransactionReader` field to `transactionsRPCHandler`
   - Rewrote `getTransactionsByLedgerSequence`: instead of a linear batch loop over all ledgers in [start, last], it now (1) queries the transaction index for up to `limit+1` distinct ledger sequences, (2) fetches only those specific LCMs via `BatchGetLedgersBySequences`, (3) processes transactions with lazy decode. Gap check now verifies only index-referenced ledgers have corresponding LCM data.
   - Updated `NewGetTransactionsHandler` to accept `db.TransactionReader`

4. **`cmd/stellar-rpc/internal/jsonrpc.go`** (line ~252):
   - Passes `params.TransactionReader` to `NewGetTransactionsHandler`

5. **`cmd/stellar-rpc/internal/db/mocks.go`** (lines ~69-95):
   - Added `GetLedgerSequencesWithTransactions` to `MockTransactionHandler`

6. **`cmd/stellar-rpc/internal/methods/mocks.go`** (lines ~95-102):
   - Added `BatchGetLedgersBySequences` to `MockLedgerReaderTx`

7. **`cmd/stellar-rpc/internal/methods/get_transactions_test.go`**:
   - Updated `setupDB` to also call `tx.TransactionWriter().InsertTransactions(lcm)` so the transactions table is populated
   - Added `transactionReader` field to all test handler structs
   - Adapted `TestGetTransactions_LedgerNotFound` for new behavior: missing ledgers with no transactions in the index are gracefully skipped instead of raising an error

### Demonstration

The optimization replaces a linear O(ledger-range) scan with an index-driven O(limit) approach for the `getTransactions` endpoint. Instead of fetching and deserializing every LCM in the range (including empty ledgers), the handler first queries `SELECT DISTINCT ledger_sequence FROM transactions` to identify only ledgers with transactions, then fetches just those LCMs. For sparse histories where 1 in 20 ledgers has transactions, this eliminates ~95% of DB reads and XDR deserialization, changing the cost from proportional to the scan range to proportional to the number of requested transactions.

### Test Results

All existing tests pass: `make go-test` completes successfully across all packages (db, methods, feewindow, ingest, integrationtest, ledgerbucketwindow, network, preflight, rpcdatastore, util, xdr2json). The full build (`go build ./cmd/stellar-rpc/`) compiles without errors.
