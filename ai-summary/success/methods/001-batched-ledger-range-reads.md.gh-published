# 001: Batched Ledger Range Reads for `getTransactions`

**Date**: 2026-04-07
**Severity**: High
**Impact**: latency / DB I/O
**Subsystem**: methods
**Final review by**: gpt-5.4, high

## Summary

`getTransactions` previously scanned sparse ledger windows by issuing one `SELECT meta ... WHERE sequence = ?` per ledger before it could even inspect the transactions in those ledgers. The reviewed change replaces that N+1 lookup pattern with bounded range reads (`BatchGetFullLedgers`) and preserved the existing missing-ledger error behavior. In independent final review, a controlled sparse-scan benchmark showed average handler latency dropping from 41.60 ms/op to 7.20 ms/op, an 82.7% reduction.

## Root Cause

The old implementation walked the retention window one ledger at a time and called `GetLedger` for every sequence it touched. That forced SQLite to do thousands of repeated point lookups and repeated result allocations on sparse scans even though the code already held a single read transaction and the DB layer already had range-read primitives.

## Reproduction

The inefficiency appears when a client paginates from an older `startLedger` across a sparse window and asks for a full page (`limit=200`). Under that access pattern, the handler may need to inspect thousands of mostly empty ledgers before finding enough transactions, so the pre-optimization path spent most of its time on repeated metadata lookups instead of transaction decoding.

## Affected Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:223-324` — old logic scanned ledgers individually while filling a page.
- `cmd/stellar-rpc/internal/db/ledger.go:getLedgerFromDB:352-367` — each scanned ledger triggered a separate `SELECT meta FROM ledger_close_meta WHERE sequence = ?`.

## Optimization

- **Files modified**:
  - `cmd/stellar-rpc/internal/db/ledger.go` — added `LedgerReaderTx.BatchGetFullLedgers` for ordered range reads that fully deserialize `LedgerCloseMeta`.
  - `cmd/stellar-rpc/internal/methods/get_transactions.go` — replaced the per-ledger fetch loop with chunked range reads and explicit gap validation.
  - `cmd/stellar-rpc/internal/methods/mocks.go` — extended the transaction-reader mock for the new interface method.
- **How to verify**:
  1. Build: `PATH="$HOME/.cargo/bin:/usr/local/go/bin:$PATH" make -j8 build-stellar-rpc`
  2. Run existing tests: `PATH="$HOME/.cargo/bin:/usr/local/go/bin:$PATH" make go-test`
  3. Benchmark: run the `stellar-rpc-blaster` getTransactions sparse-ledger sweep from the benchmarking procedure, or an equivalent controlled sparse-scan benchmark against baseline vs. optimized code on the same host.

### Changes Made

The DB layer now exposes a single-query range fetch for full `LedgerCloseMeta` values. `getTransactionsByLedgerSequence` consumes that range API in 200-ledger chunks, verifies that no ledgers are missing inside each chunk, and then processes each returned ledger in order until the page limit is satisfied. This preserves correctness while collapsing the number of SQLite round-trips from N to `ceil(N/200)` for sparse scans.

### Benchmark Results

Independent final review used the same synthetic sparse workload for both versions: a 4,000-ledger retention window with transactions present every 20th ledger, `startLedger=1`, and `limit=200`. Baseline code was compiled by overlaying the modified files with their `HEAD` versions so that only the reviewed change differed between runs. Each side was measured 5 times with `GOMAXPROCS=1`, `-cpu 1`, and `-benchtime=3s`.

| Metric | Before | After | Improvement |
|--------|--------|-------|-------------|
| Avg handler latency (`ns/op`) | 41,600,753 | 7,195,336 | 82.7% |
| Speedup | 1.00x | 5.78x | 5.78x |
| Bytes allocated/op | 13,535,404 | 4,035,422 | 70.2% |
| Allocations/op | 207,558 | 43,047 | 79.3% |
| Errors | 0 | 0 | — |

Relevant benchmark output:

```text
Optimized:
BenchmarkGetTransactionsSparseScan  498  7202707 ns/op   4035497 B/op   43047 allocs/op
BenchmarkGetTransactionsSparseScan  501  7172358 ns/op   4035423 B/op   43047 allocs/op
BenchmarkGetTransactionsSparseScan  500  7181663 ns/op   4035419 B/op   43046 allocs/op
BenchmarkGetTransactionsSparseScan  501  7185344 ns/op   4035425 B/op   43047 allocs/op
BenchmarkGetTransactionsSparseScan  499  7234608 ns/op   4035422 B/op   43047 allocs/op

Baseline:
BenchmarkGetTransactionsSparseScan   80  41374063 ns/op  13543682 B/op  207558 allocs/op
BenchmarkGetTransactionsSparseScan   86  42209533 ns/op  13535994 B/op  207595 allocs/op
BenchmarkGetTransactionsSparseScan   82  41571176 ns/op  13535276 B/op  207558 allocs/op
BenchmarkGetTransactionsSparseScan   86  41449707 ns/op  13535163 B/op  207534 allocs/op
BenchmarkGetTransactionsSparseScan   87  41399287 ns/op  13535404 B/op  207566 allocs/op
```

## Expected vs Actual Behavior

- **Expected**: `getTransactions` should walk sparse ledger ranges without paying one SQLite round-trip per ledger when it already holds a read transaction and the DB layer can fetch ledger ranges in order.
- **Actual**: the old implementation did one point lookup per scanned ledger, so sparse pages spent a large share of their time and allocations on repeated metadata fetches.

## Adversarial Review

1. Exercises claimed inefficiency: YES — the benchmark forces the handler to scan roughly 2,000 ledgers to collect a 200-transaction page, which is exactly the claimed sparse-window hot path.
2. Realistic preconditions: YES — this is workload-dependent, but older cursors on lower-activity/test networks can traverse long sparse windows before filling a page.
3. Inefficiency vs by-design: INEFFICIENCY — there is no correctness requirement for one point query per ledger, and the DB layer already supported range-oriented reads.
4. Final severity: High — independent measurements show an 82.7% latency reduction on the targeted sparse-scan workload, well above the >20% threshold.
5. In scope: YES — the change only affects the `getTransactions` call chain and its internal ledger metadata reads.
6. Benchmark methodology: CORRECT — same host, same synthetic dataset, same benchmark harness, same compiler settings, and baseline isolated via Go overlay so only the reviewed files differed.
7. Alternative explanations: NONE — the result is too large and too consistent across five runs to be explained by normal variance, and allocations dropped in the same direction.
8. Novelty: NOVEL

## Suggested Follow-Up

NONE
