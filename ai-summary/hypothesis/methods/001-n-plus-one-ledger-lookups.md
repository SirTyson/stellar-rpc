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
