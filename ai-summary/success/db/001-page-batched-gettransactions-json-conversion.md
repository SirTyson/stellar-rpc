# 001: Page-Batched `getTransactions` JSON Conversion

**Date**: 2026-04-07
**Severity**: Medium
**Impact**: latency / CGo-FFI overhead
**Subsystem**: db
**Final review by**: gpt-5.4, high

## Summary

`getTransactions` previously converted each transaction's JSON subfields one transaction at a time after parsing it, paying repeated CGo and Rust type-resolution overhead even though the full page was already buffered before the response returned. The reviewed change defers JSON work until the page is assembled and batch-converts the page in six `ConvertBytesSlice` calls. In independent final review, a warm-baseline-controlled `stellar-rpc-blaster` comparison on the same DB and seed data showed p50 latency falling by **5.93%** at 700 RPS and by **7.01%-9.70%** across the 50-500 RPS range, with larger p95/p99 wins and no loss of error-free throughput.

## Root Cause

The old JSON path crossed the Go/Rust FFI boundary repeatedly inside the hot transaction loop. Every returned transaction paid separate conversions for `TransactionResult`, `TransactionEnvelope`, and `TransactionMeta`, plus per-transaction event conversions, even though the handler already had the entire response page in memory and the xdr2json layer already exposed a batch API designed to amortize that exact overhead.

## Reproduction

Call `getTransactions` with `format=json` on pages containing many transactions. The handler parses each transaction while filling the page, then serializes multiple JSON subfields for every transaction before moving on to the next one. On dense pages, that creates hundreds of short `xdr_to_json`/`xdr_batch_to_json` crossings instead of a small fixed set of page-level conversions.

## Affected Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:152-162` — the JSON path now defers per-transaction conversion and records pending page items.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:286-365` — the handler accumulates pending transactions and batch-converts them once after the page is assembled.
- `cmd/stellar-rpc/internal/methods/json.go:pendingTxJSON/batchConvertTransactionsToJSON:94-210` — page-level batching collects all core and event byte slices and assigns converted JSON back by index.

## Optimization

- **Files modified**:
  - `cmd/stellar-rpc/internal/methods/json.go` — added `pendingTxJSON` and `batchConvertTransactionsToJSON`.
  - `cmd/stellar-rpc/internal/methods/get_transactions.go` — deferred JSON conversion until page completion and invoked the batch conversion pass.
  - `cmd/stellar-rpc/internal/methods/get_transactions_test.go` — kept JSON-path coverage for the batched response flow.
- **How to verify**:
  1. Build: `PATH="$HOME/.cargo/bin:/usr/local/go/bin:$PATH" make -j8 build-stellar-rpc`
  2. Run existing tests: `PATH="$HOME/.cargo/bin:/usr/local/go/bin:$PATH" make go-test`
  3. Benchmark: run the `stellar-rpc-blaster` getTransactions sweep on shared seed data, then rerun the baseline binary on the same warmed DB/seed as a control.

### Changes Made

The reviewed implementation stores parsed JSON-format transactions in a pending slice while the page is being built. After ledger iteration stops, it batch-converts all `Result`, `Envelope`, `Meta`, `DiagnosticEvent`, `TransactionEvent`, and `ContractEvent` payloads across the entire page, then writes the converted JSON back into the already-ordered `TransactionInfo` entries. This directly addresses the claimed page-level batching opportunity for the core fields, and the measured end-to-end gain reflects the full page-batched JSON pass that shipped with it.

### Benchmark Results

Independent final review first ran a full baseline-vs-optimized sweep, then reran the baseline binary after the optimized sweep on the same warmed DB and the same seed file to eliminate cold-cache bias. The warm-baseline rerun is the authoritative "Before" value below.

| Metric | Before | After | Improvement |
|--------|--------|-------|-------------|
| p50 latency | 7.147 ms | 6.723 ms | 5.93% |
| p95 latency | 24.623 ms | 20.271 ms | 17.67% |
| p99 latency | 29.791 ms | 24.591 ms | 17.45% |
| Max RPS (0 errors, tested) | 700 | 700 | 0.00% |
| Errors | 0 | 0 | — |

Additional controlled p50 deltas on the same warmed DB and seed:

| Target RPS | Before p50 | After p50 | Improvement |
|-----------|------------|-----------|-------------|
| 50 | 5.191 ms | 4.827 ms | 7.01% |
| 100 | 5.359 ms | 4.839 ms | 9.70% |
| 200 | 5.711 ms | 5.191 ms | 9.11% |
| 300 | 6.035 ms | 5.451 ms | 9.68% |
| 500 | 6.503 ms | 5.927 ms | 8.86% |
| 700 | 7.147 ms | 6.723 ms | 5.93% |

Relevant benchmark excerpts:

```text
Warm baseline 700 RPS:
target_rps: 700, success: 12152, errors: 0, p50.0: 7.147, p95.0: 24.623, p99.0: 29.791

Optimized 700 RPS:
target_rps: 700, success: 12123, errors: 0, p50.0: 6.723, p95.0: 20.271, p99.0: 24.591
```

## Expected vs Actual Behavior

- **Expected**: once a JSON `getTransactions` page is buffered, homogeneous XDR payloads should be converted in page-level batches instead of one transaction at a time.
- **Actual**: the old path converted the same field types repeatedly inside the hot loop, paying avoidable CGo and Rust dispatch overhead for every page item.

## Adversarial Review

1. Exercises claimed inefficiency: YES — the measured path is exactly the JSON-format `getTransactions` page assembly loop where the old code performed per-transaction conversions.
2. Realistic preconditions: YES — any client requesting JSON `getTransactions` pages on transaction-heavy ledgers hits this path.
3. Inefficiency vs by-design: INEFFICIENCY — there is no correctness requirement to cross the FFI boundary per transaction after the whole page has already been collected.
4. Final severity: Medium — the warm-baseline control preserved **5.93%-9.70%** p50 reductions and **11.40%-17.67%** p95 reductions across the tested 50-700 RPS range.
5. In scope: YES — the change only affects the `getTransactions` JSON call chain.
6. Benchmark methodology: CORRECT — same host, same futurenet-backed DB, same seed data, same blaster config, and a post-optimization warm-baseline rerun specifically added to rule out cache/startup bias from the first baseline sweep.
7. Alternative explanations: NONE MATERIAL — the initial cold baseline overstated the delta at low load, but the warm-baseline control still showed a consistent latency win at every common RPS level.
8. Novelty: NOVEL

## Suggested Follow-Up

If finer attribution matters, benchmark "core fields only" versus the shipped "core + event" page-batching pass; the measured improvement here reflects the full page-level JSON batching implementation that landed.
