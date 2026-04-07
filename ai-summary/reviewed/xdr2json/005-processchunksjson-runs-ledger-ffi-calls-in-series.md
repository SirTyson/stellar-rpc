# H005: `processChunksJSON` Still Runs Independent Ledger FFI Conversions in Series

**Date**: 2026-04-07
**Subsystem**: xdr2json
**Severity**: Medium
**Impact**: latency / CPU / multicore utilization
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once `BatchGetLedgersBySequences()` has materialized the selected ledger chunks in
memory and the SQLite snapshot has been released, JSON extraction should use
available cores across independent ledgers. A page spanning many ledgers should
not wait for one ledger's Rust conversion to finish before the next one starts.

## Mechanism

`getTransactionsByLedgerSequence()` releases the DB snapshot before CPU-heavy work,
then `processChunksJSON()` loops over `chunks` and invokes
`xdr2json.LCMTransactionsToJSON(chunk.Lcm)` strictly sequentially. Each ledger
conversion reads only its own raw bytes and produces ordered per-ledger JSON
results, so a bounded parallel fan-out across chunks could overlap independent
Rust parses and JSON extraction. This is especially attractive for sparse pages
that span many small ledgers where no single ledger has enough transactions to
benefit much from within-ledger parallelism.

## Trigger

1. Issue `getTransactions` with `format=json` against a sparse history where the
   requested page spans many ledgers with only a few transactions each.
2. Profile CPU usage while `processChunksJSON()` runs; expect one core active at a
   time during the ledger FFI loop.
3. Compare current wall time against a prototype that dispatches per-ledger
   `LCMTransactionsToJSON` calls concurrently and merges ordered results by ledger
   sequence.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:251-333` — `processChunksJSON()` iterates selected ledgers serially and calls `LCMTransactionsToJSON()` per chunk.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:432-446` — all chunks are already materialized and the DB snapshot is released before this CPU-bound phase begins.
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:45-79` — each helper call is synchronous and owns its own output buffer.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:351-377` — `lcm_transactions_to_json()` uses only function-local state.

## Evidence

The handler already has the full `chunks` slice in memory before entering
`processChunksJSON()`, and the Rust entrypoint does not share mutable batch state
across calls. That makes each per-ledger conversion a natural concurrency unit.

## Anti-Evidence

Pages dominated by one dense ledger will see little benefit from across-ledger
parallelism alone. The implementation must preserve response ordering and avoid
wasting work once the global limit is satisfied, so a naive fire-all-threads
approach would need bounds and cancellation handling.

---

## Review

**Verdict**: VIABLE
**Severity**: Medium
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated. Reviewed/001 targets six `ConvertBytesSlice` calls in `batchConvertTransactionsToJSON` (old non-processChunksJSON path). Reviewed/003 targets Go-side `ParseTransaction` parallelism in `processTransactionsInLedger` (non-JSON or cached path). Reviewed/004 targets intra-ledger Rust-side parallelism within `extract_transactions_json`. H005 uniquely targets inter-ledger Go-side parallelism of `LCMTransactionsToJSON` calls within `processChunksJSON`, the JSON-optimized path.

### Trace Summary

Traced the full `processChunksJSON` path (get_transactions.go:253-335). After `BatchGetLedgersBySequences` materializes all chunks and the SQLite snapshot is released (line 462), the JSON path enters a sequential `for _, chunk := range chunks` loop. Each iteration calls `xdr2json.LCMTransactionsToJSON(chunk.Lcm)` (line 284), which pins Go memory, invokes the Rust FFI `C.lcm_transactions_to_json()`, copies the JSON result back, and `json.Unmarshal`s it into `[]LCMTransactionJSON`. The Rust function (lib.rs:356-382) uses only function-local state — `read_xdr_to_end` into a local `LedgerCloseMeta`, `extract_transactions_json` with all-local buffers, and no global mutable state (production statics are `#[cfg(test)]` only). Each chunk's FFI call is fully independent.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:253-335` — `processChunksJSON`: sequential loop over chunks, each calling `LCMTransactionsToJSON` then assembling results into `[]TransactionInfo`; breaks at limit
- `cmd/stellar-rpc/internal/methods/get_transactions.go:458-471` — entry point: after `readTx.Done()` releases SQLite snapshot, `processChunksJSON` receives pre-materialized chunks with no remaining DB dependency
- `cmd/stellar-rpc/internal/methods/get_transactions.go:396-401` — `GetLedgerSequencesWithTransactions` already returns a tight set of ledger sequences via row-level precision, bounding the chunk count
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:45-79` — `LCMTransactionsToJSON`: pins Go memory, calls `C.lcm_transactions_to_json`, copies result via `C.GoBytes`, `json.Unmarshal`s into `[]LCMTransactionJSON` — each invocation is self-contained
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:355-382` — `lcm_transactions_to_json`: all state function-local; `catch_json_to_xdr_panic` wraps a closure with no captured mutable references; only `#[cfg(test)]` globals exist
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:477-557` — `extract_transactions_json`: builds per-transaction JSON objects using function-local `Vec<serde_json::Value>`, no shared state

### Findings

**The inefficiency exists and is in a hot path.** `processChunksJSON` is the primary JSON code path for `getTransactions` (used whenever the LCM cache doesn't cover all requested ledgers, which is always the case for JSON since the cache is explicitly skipped at line 414). The loop runs sequentially over all chunks despite each `LCMTransactionsToJSON` call being independent.

**Thread safety confirmed.** The Rust function `lcm_transactions_to_json` has no production global state. The `#[cfg(test)]` statics (`TRACK_ALLOCATIONS`, `ALLOCATED_BYTES`) are test-only. Each invocation operates on its own `CXDR` input, creates its own `LedgerCloseMeta`, and returns an independently allocated `ConversionResult`. CGo handles per-goroutine OS thread pinning automatically.

**Chunk count and per-chunk cost estimates:**
- Default limit: 50 transactions, max limit: 200 transactions
- Sparse workload (1-5 txns/ledger): 10-200 chunks per page
- Dense workload (20-50 txns/ledger): 1-10 chunks per page
- Per-chunk FFI cost (sparse, small LCM ~2-10KB): ~200-800μs (XDR parse ~50-200μs, JSON serialize ~50-200μs, CGo overhead + Go unmarshal ~100-400μs)
- Per-chunk FFI cost (dense, large LCM ~50-500KB): ~2-20ms

**Impact estimates — sparse workload (50 txns, 25 ledgers, ~500μs/chunk):**
- Sequential: ~12.5ms of FFI time
- Parallel (8 workers): ~1.6ms
- Savings: ~10.9ms
- If total request latency is ~30-50ms (sparse pages have lighter response marshaling), this is a **22-36% reduction**

**Impact estimates — dense workload (50 txns, 3 ledgers, ~10ms/chunk):**
- Sequential: ~30ms of FFI time
- Parallel (3 workers): ~10ms
- Savings: ~20ms
- If total request latency is ~60-120ms, this is a **17-33% reduction**

**Impact estimates — max limit sparse (200 txns, 100 ledgers, ~400μs/chunk):**
- Sequential: ~40ms of FFI time
- Parallel (8 workers): ~5ms
- Savings: ~35ms — potentially High severity for this workload

**Early termination waste is bounded.** `GetLedgerSequencesWithTransactions` uses row-level precision with the limit applied, so the chunk list already approximates the minimum required set. At most one trailing ledger may have excess transactions beyond the limit — the overhead of converting that one extra LCM is negligible.

**Complementary with other reviewed optimizations.** This optimization is independent of reviewed/001 (old batch path parallelism), reviewed/002 (intra-batch Rust parallelism), reviewed/003 (intra-ledger Go staging parallelism), and reviewed/004 (intra-ledger Rust transaction parallelism). H005 is the only one that addresses the `processChunksJSON` inter-ledger loop.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/methods/get_transactions.go:253-335` — `processChunksJSON`
- **Change description**: Refactor the sequential `for _, chunk := range chunks` loop into a parallel fan-out pattern:
  1. Pre-compute per-chunk metadata (ledgerSeq, closeTime, startTxIdx) sequentially — these are cheap header reads.
  2. Launch bounded goroutines (via `errgroup.Group` with `SetLimit(min(runtime.NumCPU(), 8))`) to call `xdr2json.LCMTransactionsToJSON(chunk.Lcm)` for each chunk in parallel, storing `[]LCMTransactionJSON` results in a pre-allocated indexed slice.
  3. After `errgroup.Wait()`, iterate results sequentially to build `[]protocol.TransactionInfo`, maintaining ordering and applying the start cursor + limit logic identically to the current code.
  4. Context cancellation: pass `ctx` to the errgroup to propagate cancellation to all goroutines.
- **Correctness check**: Existing `getTransactions` tests with `format=json` cover this path. Run with `-race` to verify no data races. Each goroutine only reads its own `chunk.Lcm` slice (pre-materialized, immutable) and writes to its own output slot. The sequential post-merge phase preserves cursor tracking and limit enforcement.
- **Benchmark focus**: Measure wall-clock time of the FFI loop (from first `LCMTransactionsToJSON` call to last) for pages spanning 10-50 ledgers with 1-5 txns each. Target: ~4-8× wall-time reduction on 4-8 core machines. Also measure overall `getTransactions` p50/p99 latency for sparse JSON pages. Target: ~10-30% end-to-end latency reduction. Additionally test with max limit (200 txns) over sparse history to validate near-linear speedup with chunk count.
