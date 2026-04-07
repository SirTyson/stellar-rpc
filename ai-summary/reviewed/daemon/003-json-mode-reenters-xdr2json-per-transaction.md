# H003: JSON getTransactions Re-enters xdr2json Per Transaction Instead of Per Page

**Date**: 2026-04-07
**Subsystem**: daemon
**Severity**: High
**Impact**: FFI boundary / serialization overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

For `getTransactions` in `format=json`, the daemon should batch homogeneous XDR-to-JSON conversion across the whole response page so that a 50-200 transaction response crosses the CGo/Rust boundary a small constant number of times. The hot path should not make separate FFI calls for each transaction's result, envelope, and meta when the batch API already exists.

## Mechanism

Inside `processTransactionsInLedger`, the JSON path calls `transactionToJSON(tx)` for every transaction, and that helper performs three separate `xdr2json.ConvertBytes(...)` calls (`TransactionResult`, `TransactionEnvelope`, `TransactionMeta`). Event conversion is only batched within each individual transaction, not across the full page. As a result, a 200-transaction JSON page still performs hundreds of CGo crossings and C-side input copies even though `xdr2json.ConvertBytesSlice` already exists specifically to amortize boundary overhead across many same-typed payloads.

## Trigger

Benchmark `getTransactions` with `format=json` at limits near `50` and `200`, especially on transactions that also carry diagnostic or contract events. Compare the current code to a version that accumulates `Result`, `Envelope`, `Meta`, and event byte slices for the whole page and calls `ConvertBytesSlice` once per XDR type before scattering the JSON results back into `protocol.TransactionInfo`.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:150-187` — per-transaction JSON path invokes `transactionToJSON`, `jsonifySlice`, and `BuildEventsJSONFromTransaction`.
- `cmd/stellar-rpc/internal/methods/json.go:12-36` — `transactionToJSON` does three one-item `ConvertBytes` calls per transaction.
- `cmd/stellar-rpc/internal/methods/json.go:56-90` — batching helpers already exist, but only for slices passed within a single transaction.
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:61-125` — `ConvertBytesSlice` provides the batch CGo path that can replace many single-item `ConvertBytes` calls.

## Evidence

The current JSON path performs one CGo call per `Result`, one per `Envelope`, and one per `Meta` for every transaction in the page, even though the conversion layer already has a `ConvertBytesSlice` API designed to batch same-typed payloads. That means the code is paying repeated `C.CBytes`, `xdr_to_json`, and Go/C boundary costs that scale linearly with transaction count instead of with field types.

## Anti-Evidence

This only helps `format=json`; XDR responses already stay on the Go side and use the reusable `xdr.EncodingBuffer`. The batching rewrite also needs careful bookkeeping to preserve per-transaction error attribution and to split flattened event results back into the existing response shape.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full `getTransactions` JSON path from `getTransactionsByLedgerSequence` → `processTransactionsInLedger` → `transactionToJSON` → `ConvertBytes` → CGo `xdr_to_json`, and compared it against the batch path `ConvertBytesSlice` → CGo `xdr_batch_to_json`. The inefficiency is real: for a 200-transaction page, the code makes ~600 individual CGo crossings for Result/Envelope/Meta (plus ~600 more for per-tx event batching) when ~6 batch calls would suffice. However, the hypothesis overstates the savings — the code already uses `runtime.Pinner` with zero-copy Go→C memory passing (no `C.CBytes` copies), so the only savings are from amortizing CGo boundary crossings, `C.CString` type-name allocation, and Rust-side `catch_unwind` + `TypeVariant::from_str` overhead.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:150-187` — confirmed: JSON path calls `transactionToJSON` + `jsonifySlice` + `BuildEventsJSONFromTransaction` per transaction, totaling 6 CGo crossings per tx
- `cmd/stellar-rpc/internal/methods/json.go:12-36` — confirmed: `transactionToJSON` makes 3 individual `ConvertBytes` calls (Result, Envelope, Meta)
- `cmd/stellar-rpc/internal/methods/json.go:56-90` — confirmed: `jsonifySlice`/`jsonifySliceOfSlices` use `ConvertBytesSlice` but are called per-transaction, not per-page
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:37-44` — `ConvertBytes`: calls `convertAnyBytes` which does `reflect.TypeOf`, `C.CString`, `runtime.Pinner`, `C.xdr_to_json`, per invocation
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:65-132` — `ConvertBytesSlice`: single CGo crossing via `C.xdr_batch_to_json`, amortizes type-name and boundary overhead
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:134-164` — `convertAnyBytes`: uses `runtime.Pinner` (NOT `C.CBytes`), zero-copy Go→C memory passing already in place
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:127-161` — `xdr_to_json`: per-call `catch_json_to_xdr_panic` → `catch_unwind` + `TypeVariant::from_str` + XDR parse + JSON serialize
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:198-270` — `xdr_batch_to_json`: single `catch_unwind` + single `TypeVariant::from_str`, iterates items internally
- `cmd/stellar-rpc/internal/db/transaction.go:238-278` — `ParseTransaction`: MarshalBinary for Result/Meta/Envelope + events; this intermediate serialization cost is unchanged by batching
- `cmd/stellar-rpc/internal/methods/get_transaction.go:143-156` — `BuildEventsJSONFromTransaction`: per-tx `jsonifySliceOfSlices` + `jsonifySlice`, each a separate CGo crossing

### Findings

**The inefficiency is real but the impact is modest:**

Per individual `ConvertBytes` call, the fixed overhead that batching eliminates is:
- CGo boundary crossing: ~100-150ns
- Go-side `reflect.TypeOf().Name()` + `C.CString` + `C.free`: ~80-100ns
- Rust-side `catch_unwind` + `from_c_string` + `TypeVariant::from_str`: ~100-150ns
- Total per-call fixed overhead: ~300-400ns

For a 200-transaction page: ~1200 individual CGo crossings → ~6 batch calls. Savings: ~1194 × 350ns ≈ **418µs**.

**However, the hypothesis's C.CBytes claim is incorrect.** The code already uses `runtime.Pinner` with zero-copy Go→C memory passing (conversion.go:140-148). There is no per-item C-heap allocation for XDR input data. This eliminates what would have been the largest per-item saving for large TransactionMeta blobs.

**Estimated impact by workload:**
- Classical payment pages (200 small txns, ~3-6ms total): ~7-14% improvement → borderline Medium
- Soroban transaction pages (200 large txns, ~30-100ms total): ~0.4-1.4% improvement → Low
- Mixed workloads (typical production): ~2-5% improvement → Low

**Severity downgrade rationale:** The hypothesis claims High (>20%) but the actual conversion work (XDR deserialization + JSON serialization in Rust) dominates the per-call overhead by 10-100×. The zero-copy memory passing already eliminates the biggest potential saving. For typical mixed workloads, this is a Low severity optimization.

**Correctness assessment:** The proposed batch refactor is correct and safe:
- `ConvertBytesSlice` already handles per-item errors (returns on first error)
- Memory is already held in `collectedMetas` from Phase 1 — `ParseTransaction` creates byte slices that remain valid
- Index bookkeeping to scatter batch results back to per-tx `TransactionInfo` structs is straightforward (same pattern as `jsonifySliceOfSlices`)
- No thread-safety concerns — processing is single-goroutine per request

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/methods/get_transactions.go` — refactor `processTransactionsInLedger` to collect byte slices in a first pass, batch-convert via `ConvertBytesSlice`, then scatter results. Also modify `cmd/stellar-rpc/internal/methods/json.go` to add a page-level batch conversion function.
- **Change description**: Split the JSON path in `processTransactionsInLedger` into two phases: (1) iterate transactions and call `ParseTransaction` to accumulate Result/Envelope/Meta/Events byte slices into per-type slices, (2) call `ConvertBytesSlice` once per XDR type (TransactionResult, TransactionEnvelope, TransactionMeta, DiagnosticEvent, ContractEvent, TransactionEvent), then scatter JSON results back into the `TransactionInfo` structs.
- **Correctness check**: Existing tests in `get_transactions_test.go` with `format=json` cover this path. Run `make go-test` to verify no regressions.
- **Benchmark focus**: Benchmark `getTransactions` with `format=json` and `limit=200` on a ledger range with many small classical transactions (best case for this optimization). Measure per-request latency and RPS. Expect ~5-10% improvement for small-transaction pages, <2% for Soroban-heavy pages.
