# H001: Application-order page planning never uses the transaction index

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: High
**Impact**: DB / I/O / XDR decode overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

`getTransactions` should fetch ledger blobs roughly in proportion to the transactions actually returned in the page. When the next `limit` transactions all live in one or two dense ledgers, the handler should use the existing transaction index to discover that exact page boundary first, then read only those ledgers and only the needed transaction span.

## Mechanism

The current handler plans work only in ledger space: it always starts from the cursor ledger and batch-reads a contiguous `[batchStart, batchEnd]` range from `ledger_close_meta`, then discovers after full blob reads and deserialization whether the page was already satisfied. The DB already stores `(ledger_sequence, application_order)` for every transaction, so a planner query for the next `limit` rows ordered by those columns could reveal the exact terminal ledger and terminal application order before any large LCM blobs are fetched; without that, dense pages still do O(batch width) ledger work even when the page ends inside the first ledger.

## Trigger

Load recent ledgers with 50+ transactions each, then call `getTransactions` with `limit=10` from the first ledger in the range. The response only needs the first 10 application orders from that ledger, but the handler will still read the whole first 50-ledger batch from `ledger_close_meta` before it can stop.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:203-310` — batch plan is based only on contiguous ledger ranges
- `cmd/stellar-rpc/internal/db/sqlmigrations/02_transactions.sql:4-10` — the transaction index already stores `ledger_sequence` and `application_order`
- `cmd/stellar-rpc/internal/db/transaction.go:TransactionReader/getTransactionByHash:51-53,194-235` — DB layer already uses the transaction index to locate precise transaction positions inside a ledger

## Evidence

`getTransactionsByLedgerSequence()` never consults `transactions`; it only advances `batchStart` by a fixed 50-ledger step and calls `BatchGetLedgerMetas()` for the whole range (`cmd/stellar-rpc/internal/methods/get_transactions.go:241-301`). The `transactions` table already persists `application_order` alongside `ledger_sequence` (`cmd/stellar-rpc/internal/db/sqlmigrations/02_transactions.sql:4-10`), and `getTransactionByHash()` proves the codebase can use that index to jump to a precise transaction position within a ledger (`cmd/stellar-rpc/internal/db/transaction.go:200-235`).

## Anti-Evidence

If the requested page naturally spans many ledgers or uses a limit close to the maximum, the benefit narrows because most of the scanned ledgers are still needed. Any fix has to preserve exact cursor ordering and the current missing-ledger error behavior when the local store is corrupted.

---

## Review

**Verdict**: VIABLE
**Severity**: High
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full `getTransactions` path from `getTransactionsByLedgerSequence` through `BatchGetLedgerMetas` and `processTransactionsInLedger`. Confirmed the handler walks a fixed 50-ledger batch per iteration, eagerly fetching and fully deserializing all LCMs via `l.tx.Select(ctx, &results, query)` before the inner loop discovers whether the page limit was already met. The `transactions` table with `(ledger_sequence, application_order)` and its `index_ledger_sequence` B-tree index already exists and is exercised by `getTransactionByHash`, proving the infrastructure for index-driven lookups is in place. A row-level planner query (`ORDER BY ledger_sequence, application_order LIMIT ?`) would give exact page boundaries before any LCM blob I/O.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:241-301` — `batchSize=50` hardcoded; outer loop walks `batchStart` in fixed steps calling `BatchGetLedgerMetas` for the entire range before processing any transactions
- `cmd/stellar-rpc/internal/db/ledger.go:BatchGetLedgerMetas:120-140` — `SELECT meta FROM ledger_close_meta WHERE sequence >= ? AND sequence <= ? ORDER BY sequence ASC` into `[]xdr.LedgerCloseMeta`; each row triggers `LedgerCloseMeta.Scan` → `UnmarshalBinary`, a full recursive XDR deserialisation
- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:75-199` — creates `ingest.NewLedgerTransactionReaderFromLedgerCloseMeta` per ledger (re-parses LCM), seeks to start position, iterates transactions; returns `done=true` only after limit is met
- `cmd/stellar-rpc/internal/methods/get_transactions.go:289-300` — inner loop breaks on `done`, outer loop breaks on `done`, but deserialization of all 50 LCMs already completed at line 263
- `cmd/stellar-rpc/internal/db/transaction.go:getTransactionByHash:200-235` — existing index-driven path joins `transactions t` with `ledger_close_meta lcm ON (t.ledger_sequence = lcm.sequence)`, proving the codebase already uses the index to locate precise positions
- `cmd/stellar-rpc/internal/db/sqlmigrations/02_transactions.sql:4-10` — schema: `CREATE INDEX index_ledger_sequence ON transactions(ledger_sequence)`, the existing B-tree index

### Findings

1. **The inefficiency is confirmed and significant for dense ledgers.** With `limit=10` and dense ledgers (50+ txns each), the first ledger satisfies the entire page. But `BatchGetLedgerMetas` has already fetched and fully deserialized all 50 LCMs in the batch — 49 of which are immediately discarded. Each dense LCM can be hundreds of KB to megabytes of allocated Go objects, making this a substantial CPU and memory waste.

2. **Row-level precision is uniquely valuable for dense scenarios.** A planner query like `SELECT ledger_sequence FROM transactions WHERE ledger_sequence >= ? ORDER BY ledger_sequence, application_order LIMIT ?` returns the exact set of ledger sequences needed to satisfy the page. For dense ledgers where 10 transactions fit in 1 ledger, this returns a single ledger_sequence value vs the current approach of fetching 50. For sparse ledgers, it naturally skips empty ones. This single query subsumes both dense and sparse optimisation.

3. **The existing index supports the planner query efficiently.** The `index_ledger_sequence` B-tree on `transactions(ledger_sequence)` enables an efficient index scan for the planner. For the query `SELECT DISTINCT ledger_sequence FROM transactions WHERE ledger_sequence >= ? ORDER BY ledger_sequence, application_order LIMIT ?`, SQLite performs an index scan + partial sort (limited by LIMIT), which is O(limit) in practice — far cheaper than reading 50 multi-hundred-KB LCM blobs.

4. **Waste quantification**: In the trigger scenario (dense ledgers, limit=10), the handler performs: (a) 1 SQL query reading ~50 LCM blobs from SQLite (potentially 5–50MB of I/O), (b) 50 full XDR deserializations into Go heap objects, (c) 50 `LedgerTransactionReader` constructions, then uses only 1 ledger's worth. That's ~98% wasted batch work. Even with warm page cache, the CPU cost of 49 unnecessary recursive XDR unmarshals dominates.

5. **Correctness constraints are manageable.** The cursor format (TOID) already encodes `(ledger_sequence, transaction_order)`, which maps directly to the `(ledger_sequence, application_order)` columns in the index. Gap detection (lines 271-287) would need adaptation — instead of checking contiguity across 50 ledgers, it would verify that the specific ledgers returned by the planner exist in `ledger_close_meta`. This is a valid trade-off: the retention window already guarantees contiguity during normal operation, and the planner approach can verify existence of its specific target ledgers.

6. **Overlap note**: This hypothesis is closely related to in-flight H002 (sparse-scan-ignores-transaction-index), which proposes using `SELECT DISTINCT ledger_sequence` at ledger granularity. The current hypothesis goes further by using row-level (application_order) precision, which is more valuable for dense ledgers where a single ledger may contain the entire page. The two are complementary — a PoC could implement either or both, but the row-level approach subsumes the ledger-level approach.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence` (lines 237-301) and `cmd/stellar-rpc/internal/db/transaction.go` (new method on `transactionHandler`)
- **Change description**: Add a new method to `TransactionReader` interface and `transactionHandler`: `GetPageLedgerSequences(ctx context.Context, startSeq uint32, startAppOrder int, limit int) ([]uint32, error)` that executes `SELECT DISTINCT ledger_sequence FROM transactions WHERE (ledger_sequence > ? OR (ledger_sequence = ? AND application_order >= ?)) ORDER BY ledger_sequence, application_order LIMIT ?` and returns the distinct ledger sequences. In `getTransactionsByLedgerSequence`, replace the fixed-batch loop with: (1) call the planner to get the target ledger sequences, (2) fetch only those specific LCMs (either individually or with an `IN (?)` clause), (3) process them with the existing `processTransactionsInLedger`. The `transactionsRPCHandler` struct (line 23) needs a new `transactionReader db.TransactionReader` field. For gap detection, verify each returned ledger exists in `ledger_close_meta` by checking the returned LCM count matches the requested set.
- **Correctness check**: Existing tests in `cmd/stellar-rpc/internal/methods/get_transactions_test.go` cover pagination, cursor semantics, and edge cases. Verify that cursor encoding/decoding (TOID format) and the gap-detection error path remain correct. A composite index `ON transactions(ledger_sequence, application_order)` would make the planner query faster but is not required — the existing `index_ledger_sequence` is sufficient.
- **Benchmark focus**: Measure `getTransactions` latency with `limit=10` over a range where each ledger has 50+ transactions. Compare current (fetch 50 LCMs, deserialize all) vs planner (query index, fetch 1 LCM). Expect >90% reduction in LCM fetches/deserializations and >50% end-to-end latency reduction for this scenario. Also test with sparse histories and large limits to confirm no regression.

---

## PoC Attempt

**Result**: POC_PASS
**Date**: 2026-04-07
**PoC by**: claude-opus-4-6, high

### Changes Made

1. **`cmd/stellar-rpc/internal/db/transaction.go:51-59`** — Updated `TransactionReader` interface: `GetLedgerSequencesWithTransactions` now accepts `startApplicationOrder int` parameter for row-level cursor precision.

2. **`cmd/stellar-rpc/internal/db/transaction.go:170-200`** — Rewrote `GetLedgerSequencesWithTransactions` implementation to use a subquery with row-level precision. The inner query `SELECT DISTINCT ledger_sequence, application_order FROM transactions WHERE (ledger_sequence > ? OR (ledger_sequence = ? AND application_order >= ?)) AND ledger_sequence <= ? ORDER BY ledger_sequence, application_order LIMIT ?` counts distinct transactions (not distinct ledgers), then the outer query extracts the minimal set of ledger sequences. This deduplicates fee-bump hash entries (which share the same ledger_sequence/application_order) and gives exact page boundaries.

3. **`cmd/stellar-rpc/internal/db/mocks.go:80-101`** — Updated `MockTransactionHandler.GetLedgerSequencesWithTransactions` signature to match the new interface.

4. **`cmd/stellar-rpc/internal/methods/get_transactions.go:301-306`** — Updated the caller to pass `int(start.TransactionOrder)` as the `startApplicationOrder` and removed the `+1` overfetch from the limit (row-level precision handles cursor edge cases natively).

### Demonstration

The planner query now uses `(ledger_sequence, application_order)` precision instead of counting distinct ledger sequences. For a `limit=10` request against dense ledgers (50+ txns each), the inner LIMIT applies to transaction rows, returning only 1 ledger sequence instead of the previous 11 (`limit+1` distinct ledgers). This eliminates ~90% of unnecessary LCM blob fetches and XDR deserializations in the dense-ledger scenario, while degrading gracefully to the same behavior for sparse histories.

### Test Results

All 12 Go test packages pass with `-race` enabled:
- `cmd/stellar-rpc/internal/methods` — all tests pass (pagination, cursor semantics, JSON format, edge cases including missing ledgers and empty results)
- `cmd/stellar-rpc/internal/db` — all tests pass (transaction CRUD, batch ingestion, fee-bump handling)
- All other packages (`config`, `feewindow`, `ingest`, `integrationtest`, `ledgerbucketwindow`, `network`, `preflight`, `rpcdatastore`, `util`, `xdr2json`) — all pass
