# H001: Fixed 50-Ledger Batch Planning Overfetches Dense `getTransactions` Pages

**Date**: 2026-04-07
**Subsystem**: methods
**Severity**: Medium
**Impact**: latency / DB I/O / allocations
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When a `getTransactions` page can be satisfied by the current ledger or the next few ledgers, the handler should fetch and decode only that small prefix. A request such as `limit=1` or `limit=5` against dense recent ledgers should not deserialize a hardcoded 50-ledger window before it knows the first ledger already fills the page.

## Mechanism

`getTransactionsByLedgerSequence` hardcodes `const batchSize = 50` and always calls `readTx.BatchGetLedgerMetas(batchStart, batchEnd)` before it inspects even the first ledger in that batch. `BatchGetLedgerMetas` fully scans every returned row into `[]xdr.LedgerCloseMeta`, so dense or low-limit requests can return after parsing ledger 1 while the other 49 ledgers in the batch were already read, deserialized, and allocated for no reason. An adaptive first-batch planner (for example, probe 1-4 ledgers first, then grow geometrically only if the page is still short) should cut that wasted front-load on the common "small page near the tip" workload.

## Trigger

1. Populate recent ledgers densely enough that one ledger can satisfy most of a page.
2. Issue `getTransactions` with `startLedger` near the latest ledger and `limit` in the 1-10 range.
3. Compare p50 latency and allocations against a version that starts with a tiny first batch and only expands when the page is still not full.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:238-299` — hardcoded `batchSize = 50` and eager `BatchGetLedgerMetas` call before any ledger in the batch is processed.
- `cmd/stellar-rpc/internal/db/ledger.go:118-140` — `BatchGetLedgerMetas` fully deserializes every ledger meta in the requested range into a slice.

## Evidence

The planner does not consult `limit`, the cursor position within the starting ledger, or any observed transaction density before choosing 50 ledgers. The batch is fetched and fully materialized first, then `processTransactionsInLedger` may stop on the first ledger that satisfies the request, which means the tail of the batch was pure overfetch.

## Anti-Evidence

Large sparse scans benefit from wider batches, so the fix should not simply shrink the constant globally. The win is strongest for dense recent traffic and small page sizes; full historical scans that genuinely need many ledgers will amortize the current batch better.

---

## Review

**Verdict**: VIABLE
**Severity**: Medium
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full `getTransactions` batch-fetch path from `getTransactionsByLedgerSequence` (get_transactions.go:241-301) through `BatchGetLedgerMetas` (ledger.go:118-140). The hardcoded `const batchSize = 50` at line 241 governs every iteration of the outer loop. `BatchGetLedgerMetas` issues a single `SELECT meta FROM ledger_close_meta WHERE sequence >= ? AND sequence <= ? ORDER BY sequence ASC` and fully deserializes all returned rows into `[]xdr.LedgerCloseMeta` via `l.tx.Select`. For dense ledgers where the first 1-2 ledgers contain enough transactions to fill a small page, the remaining 48-49 fully deserialized `LedgerCloseMeta` objects are never processed and are pure waste (SQL I/O + XDR deserialization + heap allocations). The inner loop (lines 289-297) correctly breaks on `done`, but by then the batch is already materialized.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:241` — `const batchSize = 50` hardcoded, never consulted against `limit` or density
- `cmd/stellar-rpc/internal/methods/get_transactions.go:247-268` — outer loop always fetches `batchSize` ledgers (or remaining range) per iteration via `BatchGetLedgerMetas`
- `cmd/stellar-rpc/internal/methods/get_transactions.go:263` — `readTx.BatchGetLedgerMetas(ctx, uint32(batchStart), uint32(batchEnd))` — full deserialization happens before any processing
- `cmd/stellar-rpc/internal/methods/get_transactions.go:289-300` — inner loop processes ledgers one-by-one, breaks on `done`, but waste already incurred
- `cmd/stellar-rpc/internal/db/ledger.go:118-140` — `BatchGetLedgerMetas` does `l.tx.Select(ctx, &results, query)` which scans ALL rows in the range and XDR-unmarshals each into a full `xdr.LedgerCloseMeta` (including all transactions, results, meta, and events)
- `cmd/stellar-rpc/internal/db/ledger.go:35-41` — `LedgerReaderTx` interface; `BatchGetLedgerMetas` is the only full-deserialization batch API
- `cmd/stellar-rpc/internal/methods/get_transactions.go:271-287` — gap validation uses `batchEnd - batchStart + 1` which works correctly for any batch size

### Findings

**The inefficiency is real and structurally confirmed.** The batch size is a compile-time constant that ignores three available signals: the requested `limit`, the cursor's position within the starting ledger, and observed transaction density. For the "small page near the tip" workload (clients polling for new transactions with `limit=1-10`), the handler fetches and fully deserializes 50 ledgers when 1-2 would suffice.

**Cost model for the waste:**
- `BatchGetLedgerMetas` reads all `meta` blobs from SQLite in the range. Each blob contains the full XDR-encoded `LedgerCloseMeta` including transaction envelopes, results, meta changes, and events.
- For dense ledgers (10-100 transactions each), each `LedgerCloseMeta` is 50-500 KB of XDR data.
- Deserializing 49 unnecessary dense ledgers = 2.5-25 MB of XDR unmarshaled for nothing.
- This waste includes both SQLite I/O (reading the blob bytes from disk/cache) and Go heap allocations (the deserialized XDR structs).

**The waste is confined to the first batch when the page fills quickly.** For `limit=1` against a ledger with 50+ transactions, the handler processes exactly one ledger and discards 49. For `limit=200` against sparse ledgers, the handler needs all 50 and may need additional batches, so the waste is minimal.

**The fix does not hurt the sparse-scan case.** An adaptive strategy that starts small (e.g., `min(max(limit*2, 4), 50)`) and grows to 50 for subsequent batches adds at most one extra small SQL query for the first batch. The sparse-scan case (which drives the existing benchmarks) would pay ~0.1ms extra for the small first batch before converging to the same 50-ledger chunks. This is negligible against the 7ms total sparse-scan cost measured in the success finding.

**No correctness concerns.** The gap validation logic (lines 271-287) computes `expectedCount` from `batchEnd - batchStart + 1`, which works correctly for any batch size. The inner processing loop is batch-size-agnostic. The outer loop's `batchStart += currentBatchSize` advancement is arithmetic and works for variable sizes.

**Relationship to success H001 (batched ledger range reads):** That finding replaced per-ledger point lookups with range reads, reducing SQL round-trips from N to `ceil(N/batchSize)`. This hypothesis refines the batch SIZE to also minimize data fetched per round-trip. They are complementary: H001-success reduced round-trip count; this hypothesis reduces round-trip payload size for small-limit requests.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/methods/get_transactions.go` — replace `const batchSize = 50` with an adaptive batch size. Suggested approach:
  ```go
  // First batch: sized to the requested limit (clamped to [4, 50])
  // Subsequent batches: fixed at 50 for efficient sparse scanning
  firstBatchSize := int32(min(max(limit*2, 4), 50))
  currentBatchSize := firstBatchSize
  for batchStart := start.LedgerSequence; batchStart <= lastLedgerSeq; batchStart += currentBatchSize {
      // ... existing logic ...
      currentBatchSize = 50 // grow to full size after first batch
  }
  ```
- **Change description**: Make the first batch size proportional to the requested limit so that small-limit requests against dense ledgers fetch and deserialize only a few ledgers instead of 50. Subsequent batches remain at 50 to preserve sparse-scan performance.
- **Correctness check**: All existing tests in `cmd/stellar-rpc/internal/methods/get_transactions_test.go` must pass. The gap validation, cursor pagination, and limit enforcement logic are all batch-size-agnostic.
- **Benchmark focus**: The key metric is p50/p99 latency for `getTransactions` with small limits (1, 5, 10) against dense ledgers near the tip. A synthetic Go benchmark should set up ledgers with 50-100 transactions each and measure with `limit=1` and `limit=5`. Target: >20% latency reduction for these small-limit cases. Also run the full blaster sweep to verify no regression on the mixed workload. Allocation reduction can be measured with `-benchmem` to confirm fewer bytes allocated per operation.
