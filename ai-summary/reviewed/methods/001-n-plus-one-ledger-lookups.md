# H001: getTransactions Scans Ledger Range With N+1 Point Lookups

**Date**: 2026-04-06
**Subsystem**: methods
**Severity**: High
**Impact**: latency / DB I/O
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When `getTransactions` must scan across many ledgers to collect the next page of up to 50-200 transactions, it should fetch ledger metadata in a batched or streamed read pattern inside the existing read transaction. A request that spans a sparse retention window should not pay one SQLite query per ledger before it even starts decoding transactions.

## Mechanism

`getTransactionsByLedgerSequence` iterates from the cursor ledger to `ledgerRange.LastLedger.Sequence` and calls `fetchLedgerData` for every ledger in that range. `fetchLedgerData` delegates to `readTx.GetLedger`, which issues `SELECT meta FROM ledger_close_meta WHERE sequence = ?` for each ledger, so sparse requests incur an N+1 query pattern even though `LedgerReaderTx` already exposes `BatchGetLedgers` for range reads. On long scans, the repeated SQLite round-trips and per-ledger XDR unmarshalling should dominate request time before transaction parsing begins.

## Trigger

1. Populate a retention window with thousands of ledgers where only a small fraction contain transactions.
2. Call `getTransactions` with `startLedger` near the oldest retained ledger and `limit=50` or `limit=200`.
3. Compare latency and query count against an implementation that batches ledger metadata reads (for example via `BatchGetLedgers` in bounded chunks).

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:71-88` — `fetchLedgerData` performs one lookup per ledger.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:253-276` — outer pagination loop scans ledgers one-by-one until the page is full.
- `cmd/stellar-rpc/internal/db/ledger.go:72-115` — `BatchGetLedgers` already exists as a range-fetch primitive but is unused here.
- `cmd/stellar-rpc/internal/db/ledger.go:324-340` — `getLedgerFromDB` is the per-ledger point query on the hot path.

## Evidence

The hot path uses a single read transaction, but it still does one SQL lookup for each scanned ledger. The codebase already contains a batch-oriented ledger reader for `getLedgers`, which means the lower-level DB layer has an optimization primitive that `getTransactions` is not taking advantage of.

## Anti-Evidence

If the requested page is satisfied by one or two dense ledgers, batching will help less because the loop exits quickly. The `max-transactions-limit` of 200 also caps total returned transactions, so the worst-case win is driven by sparse-ledger scans rather than dense ones.

---

## Review

**Verdict**: VIABLE
**Severity**: Medium
**Date**: 2026-04-06
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

The N+1 query pattern is confirmed. `getTransactionsByLedgerSequence` (get_transactions.go:253-276) iterates ledger-by-ledger from the cursor to the end of the retention window, calling `fetchLedgerData` per iteration. Each call dispatches to `getLedgerFromDB` (ledger.go:326-341) which builds a fresh squirrel query (`SELECT meta FROM ledger_close_meta WHERE sequence = ?`), executes it through `db.Select`, and fully deserializes the `xdr.LedgerCloseMeta` result. No prepared statement caching is used on this path. Meanwhile, `BatchGetLedgers` (ledger.go:72-115) is already available on the `LedgerReaderTx` interface and is actively used by the `getLedgers` handler (get_ledgers.go:205), but `getTransactions` does not use it.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:253-276` — Main loop iterates `for ledgerSeq := start.LedgerSequence; ledgerSeq <= int32(ledgerRange.LastLedger.Sequence)`, calling `fetchLedgerData` per ledger. No batching, no early skip for empty ledgers.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:71-88` — `fetchLedgerData` delegates to `readTx.GetLedger(ctx, ledgerSeq)`, a single-row point query.
- `cmd/stellar-rpc/internal/db/ledger.go:326-341` — `getLedgerFromDB` builds a squirrel query, calls `db.Select` (no prepared statement), allocates `[]xdr.LedgerCloseMeta`, and deserializes. Per-call overhead: SQL compilation + B-tree seek + full XDR unmarshal + Go allocations.
- `cmd/stellar-rpc/internal/db/ledger.go:72-115` — `BatchGetLedgers` does a single range query (`WHERE sequence >= ? AND sequence <= ?`), reads raw bytes, and partially deserializes (header only). This proves the DB layer supports batch reads.
- `cmd/stellar-rpc/internal/db/ledger.go:166-192` — `StreamLedgerRange` on `LedgerReader` (non-tx) uses a streaming range query with full `LedgerCloseMeta` deserialization, but is not available on `LedgerReaderTx`.
- `cmd/stellar-rpc/internal/methods/get_ledgers.go:204-214` — `fetchFromLocalDB` in `getLedgers` demonstrates how `BatchGetLedgers` is correctly integrated in an existing handler.

### Findings

1. **The N+1 pattern is real and unmitigated.** Each ledger fetch compiles a new SQL statement, performs an independent B-tree seek, allocates a fresh result slice, and does full XDR deserialization. No prepared statement cache, no query reuse, no batching on this path.

2. **Batch primitive already exists.** `LedgerReaderTx.BatchGetLedgers` performs a single range query. However, it returns `[]LedgerMetadataChunk` (header + raw bytes) rather than `[]xdr.LedgerCloseMeta`, so it's not a drop-in replacement. A new method or adaptation is needed.

3. **Streaming primitive exists but not on tx interface.** `StreamLedgerRange` on `LedgerReader` does exactly what's needed (range query with full LCM deserialization) but operates outside a transaction. Adding a similar method to `LedgerReaderTx` would be straightforward.

4. **Impact is workload-dependent.** For sparse ledgers (0-1 tx/ledger), scanning to fill a page of 200 transactions could require 200-1000+ point queries. For dense ledgers (100+ tx/ledger), the loop exits after 2-3 iterations and the overhead is negligible. The worst case is a `getTransactions` call starting near the oldest retained ledger on a testnet or low-activity network.

5. **Severity assessment: Medium, not High.** Per-ledger XDR deserialization (which occurs regardless of query pattern) is likely the dominant cost. The SQL overhead (compilation + B-tree seek) is significant but not the majority of per-ledger time. A batch query eliminates N-1 redundant SQL compilations and B-tree seeks, and improves memory locality, yielding an estimated 5-20% latency reduction on sparse scans. Dense scans see negligible improvement since the loop exits quickly.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/db/ledger.go` (add a new `BatchGetFullLedgers` or `StreamLedgerRangeTx` method to `LedgerReaderTx`) and `cmd/stellar-rpc/internal/methods/get_transactions.go` (replace the per-ledger loop with chunked batch fetches).
- **Change description**: Add a method to `LedgerReaderTx` that performs a range query returning `[]xdr.LedgerCloseMeta` (like `StreamLedgerRange` but within a transaction). Modify `getTransactionsByLedgerSequence` to fetch ledgers in bounded chunks (e.g., 200 at a time) using this new method, iterating through the batch results to process transactions until the page limit is reached.
- **Correctness check**: Existing tests in `cmd/stellar-rpc/internal/methods/get_transactions_test.go` cover pagination, cursor handling, and ledger boundary conditions. The `MockLedgerReaderTx` already mocks `BatchGetLedgers` and would need a new mock for the new method. The benchmark `BenchmarkBatchGetLedgers` in `cmd/stellar-rpc/internal/db/ledger_test.go` can be extended to compare batch vs point query performance.
- **Benchmark focus**: Measure `getTransactions` latency on a database with 1000+ ledgers where <10% contain transactions, using `limit=200` and `startLedger` near the oldest retained ledger. The metric to improve is end-to-end request latency, with a target of 5-20% reduction. A secondary metric is SQLite query count (should drop from N to ceil(N/chunk_size)).
