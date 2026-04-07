# H003: Sparse `getTransactions` Walks Keep Using 50-Ledger Batches After the First Probe

**Date**: 2026-04-07
**Subsystem**: methods
**Severity**: Medium
**Impact**: latency / DB I/O / allocations
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

After the first batch shows that a request is traversing a sparse historical window, `getTransactions` should expand subsequent range reads to the batch size that best amortizes sparse-scan overhead. The dense-page safeguard belongs on the first probe, not on every later sparse batch.

## Mechanism

`getTransactionsByLedgerSequence` now uses a fixed `const batchSize = 50` for the entire request. That keeps the first batch reasonably small for dense, low-limit pages, but it also means a long sparse walk still pays one `BatchGetLedgerMetas` call, one missing-ledger validation pass, and one `[]xdr.LedgerCloseMeta` materialization every 50 ledgers. The earlier successful batch-range optimization was benchmarked with much wider chunks for sparse scans; a hybrid planner that keeps the first batch small and then grows later batches (for example to 200) should preserve dense-page wins while reducing follow-on sparse-scan overhead.

## Trigger

1. Populate a sparse retention window where the handler must cross hundreds or thousands of ledgers to fill a page.
2. Call `getTransactions` with `limit=200` from an older `startLedger`.
3. Compare latency and allocation profiles against a version that uses a small first batch and then increases follow-on batches to 200 once the request has proven sparse.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:241-263` — a single hardcoded `batchSize = 50` governs every outer-loop iteration.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:271-287` — every batch repeats the same missing-ledger validation work.
- `cmd/stellar-rpc/internal/db/ledger.go:118-140` — each batch call materializes a fresh slice of fully decoded `LedgerCloseMeta` values.

## Evidence

The current code has no notion of "first batch" versus "follow-on batch"; once the request enters a sparse scan, it still keeps issuing 50-ledger reads until completion. That trades away a portion of the sparse-scan gain already demonstrated by the range-read optimization in favor of protecting the initial dense-page case on every iteration, even after the workload shape is known.

## Anti-Evidence

If the batch grows too early, dense small-limit requests will reintroduce the overfetch already identified in `reviewed/001-fixed-batch-overfetches-dense-pages.md`. The improvement is therefore workload-dependent and hinges on a hybrid strategy rather than a simple global constant change.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full batch loop in `getTransactionsByLedgerSequence` (get_transactions.go:241-301) and the underlying `BatchGetLedgerMetas` (ledger.go:118-140). Each iteration of the outer loop incurs a fixed per-batch overhead: Squirrel query construction, SQLite query execution (B-tree seek + cursor setup), and Go-level slice allocation for the result set. Using the success H001 benchmark data (baseline 41.6ms for 4000 per-ledger queries vs 7.2ms for 80 batch-of-50 queries), the per-query fixed overhead is ~8.8μs. Growing follow-on batches from 50 to 200 for a 4000-ledger sparse scan would reduce batch count from 80 to ~21, saving ~0.5ms (~7%) off the 7.2ms total.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:241` — `const batchSize = 50` is a compile-time constant, never adaptive
- `cmd/stellar-rpc/internal/methods/get_transactions.go:247` — `batchStart += batchSize` governs the outer loop stride; changing to a variable works without correctness issues
- `cmd/stellar-rpc/internal/methods/get_transactions.go:258-261` — `batchEnd` clamping is arithmetic and works for any batch size
- `cmd/stellar-rpc/internal/methods/get_transactions.go:263` — `BatchGetLedgerMetas(ctx, start, end)` is the per-batch SQL call; each invocation constructs a Squirrel query, executes it, and scans results into `[]xdr.LedgerCloseMeta`
- `cmd/stellar-rpc/internal/methods/get_transactions.go:271-287` — gap validation: only runs when `len(ledgers) != expectedCount`; no overhead in the normal (no-gap) case regardless of batch size
- `cmd/stellar-rpc/internal/db/ledger.go:118-140` — `BatchGetLedgerMetas` builds a new Squirrel SELECT, executes via `l.tx.Select`, returns freshly allocated slice; the per-call fixed overhead is the SQL parsing/planning + B-tree seek
- `cmd/stellar-rpc/internal/db/ledger.go:127-133` — Squirrel query builder creates a new SQL string and binds each call (no prepared statement reuse)

### Findings

**The inefficiency is real but small.** Each batch iteration pays a fixed SQL overhead (~8.8μs derived from benchmark data) that is independent of the number of rows returned. For long sparse scans, reducing the number of batch iterations from 80 to ~21 eliminates ~60 redundant query setups, saving approximately 0.5ms.

**Quantitative estimate from benchmark data:**
- Success H001 benchmark: 4000 ledgers, baseline 41.6ms (4000 queries) → optimized 7.2ms (80 queries)
- Per-query fixed overhead: (41.6 - 7.2) / (4000 - 80) ≈ 8.78μs
- Current SQL overhead for sparse scan: 80 × 8.78μs = 0.70ms
- Proposed SQL overhead: ~21 × 8.78μs = 0.18ms
- Savings: ~0.52ms, or ~7.2% of the 7.2ms total

**This is the upper bound.** The 7% figure applies to the ideal sparse-scan benchmark case (4000 ledgers, limit=200, transactions every 20th ledger). Shorter scans, higher-density data, or lower limits all reduce the number of batches and proportionally reduce the savings. Real-world mixed workloads would see a diluted effect, likely under 5%.

**No correctness concerns.** The `batchEnd` clamping (lines 258-261), gap validation (lines 271-287), inner processing loop (lines 289-297), and cursor advancement are all batch-size-agnostic. Making `batchSize` a variable instead of a constant introduces no new failure modes.

**Memory impact is benign.** Batch=200 means up to 200 `LedgerCloseMeta` objects in flight per batch. For sparse ledgers (few/no transactions), each blob is small (1-5KB). Even for dense ledgers, 200 deserialized objects is well within normal memory bounds.

**Complementary to reviewed H001.** That finding proposes a small first batch for dense pages; this proposes larger follow-on batches for sparse scans. They combine naturally: first batch = `min(max(limit*2, 4), 50)` per H001, subsequent batches = 200 per H003.

**Severity downgrade rationale:** The hypothesis claims Medium severity. The ~7% improvement on the synthetic sparse benchmark is at the boundary of the 5-20% Medium range, but this represents the best-case scenario. Real-world impact across mixed getTransactions workloads would be under 5%, placing it in Low territory. The improvement is real and measurable in isolation but represents diminishing returns after the substantial batch optimization already in place.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/methods/get_transactions.go` — replace `const batchSize = 50` with a variable that starts at 50 and grows to 200 after the first batch. If combining with reviewed H001, the first batch uses the adaptive size and subsequent batches use 200.
  ```go
  const followOnBatchSize int32 = 200
  currentBatchSize := int32(50) // or adaptive per H001
  for batchStart := start.LedgerSequence; batchStart <= lastLedgerSeq; batchStart += currentBatchSize {
      // ... existing logic ...
      currentBatchSize = followOnBatchSize // grow after first batch
  }
  ```
- **Change description**: Make follow-on batch size larger (200) to reduce per-batch SQL overhead for long sparse scans. The first batch remains small to protect the dense/low-limit case.
- **Correctness check**: All existing tests in `cmd/stellar-rpc/internal/methods/get_transactions_test.go` must pass. The gap validation, pagination, and limit enforcement are batch-size-agnostic.
- **Benchmark focus**: Run `BenchmarkGetTransactionsSparseScan` (the same synthetic benchmark from success H001) with batch=50 vs follow-on batch=200. Target: ~5-7% latency improvement for the 4000-ledger sparse scan. Allocation reduction should also be measurable (~60 fewer slice allocations). Also verify no regression on dense small-limit queries with the blaster.
