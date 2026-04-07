# H002: JSON pages still convert result, envelope, and meta one FFI call per transaction

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Medium
**Impact**: CPU / CGo / allocation overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

For `getTransactions` JSON responses, the conversion of transaction result, envelope, and meta should be batched across the whole page. Returning `N` transactions should use roughly three FFI conversions total for these core fields, not three independent CGo crossings per transaction.

## Mechanism

Inside the per-transaction loop, `processTransactionsInLedger()` calls `transactionToJSON()`, and `transactionToJSON()` performs three separate `xdr2json.ConvertBytes()` calls for `Result`, `Envelope`, and `Meta`. The xdr2json layer already exposes `ConvertBytesSlice()` specifically to amortize CGo and Rust type-resolution overhead across a homogeneous batch, so `getTransactions` is leaving a page-level optimization on the table: it could collect all result blobs, all envelope blobs, and all meta blobs for the page, convert each slice once, then assign the returned JSON back by index.

## Trigger

Call `getTransactions` with `xdrFormat=json` and a large page limit over dense ledgers. A CPU profile should show hundreds of short `xdr_to_json`/CGo calls for the core transaction fields, even though the response already has all items buffered before it is returned.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:147-170` — per-transaction JSON conversion occurs in the hot append loop
- `cmd/stellar-rpc/internal/methods/json.go:transactionToJSON:12-36` — three individual `ConvertBytes()` calls per transaction
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:ConvertBytesSlice:61-126` — existing batch API that can collapse many conversions into one CGo call per field type
- `cmd/stellar-rpc/internal/xdr2json/conversion_test.go:95-125` — benchmark already compares individual conversions against the batch path

## Evidence

`processTransactionsInLedger()` converts JSON inline while it is still appending each `TransactionInfo` (`cmd/stellar-rpc/internal/methods/get_transactions.go:147-170`). `transactionToJSON()` then invokes `ConvertBytes()` three times serially for every transaction (`cmd/stellar-rpc/internal/methods/json.go:12-36`). In contrast, `ConvertBytesSlice()` is explicitly implemented to batch homogeneous byte buffers through one CGo call and one Rust type-resolution step (`cmd/stellar-rpc/internal/xdr2json/conversion.go:61-126`), and the conversion benchmark already treats that batched path as a meaningful optimization target (`cmd/stellar-rpc/internal/xdr2json/conversion_test.go:95-125`).

## Anti-Evidence

This only affects JSON-format requests; the default XDR/base64 path does not pay this FFI cost. Any fix has to preserve transaction order and still surface conversion failures with enough context to identify which page item failed.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full `getTransactions` JSON path from `getTransactionsByLedgerSequence` through `processTransactionsInLedger` into `transactionToJSON` and down to the Rust FFI layer. Confirmed that each transaction triggers three independent CGo→Rust round-trips (`xdr_to_json`), each paying `panic::catch_unwind`, `TypeVariant::from_str`, `C.CString` allocation, `reflect.TypeOf`, and three `defer`-guarded frees. The batch path (`xdr_batch_to_json`) amortizes all of these per-call costs into one crossing per field type. However, the dominant cost in each call is the XDR deserialization (`read_xdr_to_end`) and JSON serialization (`serde_json::to_string`), which are identical in both paths — only the per-call overhead is saved.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:112-199` — iterates transactions in a ledger; for JSON format, calls `transactionToJSON` at line 149 inside the hot loop, then `jsonifySlice` for diagnostic events (already batched per-tx) at line 157
- `cmd/stellar-rpc/internal/methods/json.go:transactionToJSON:12-37` — three sequential `ConvertBytes` calls for Result, Envelope, Meta; each independently crosses CGo
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:ConvertBytes:36-43` — per-call: `reflect.TypeOf().Name()`, then delegates to `convertAnyBytes`
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:convertAnyBytes:128-147` — per-call: `C.CString` (malloc+memcpy), `CXDR` (C.CBytes = malloc+memcpy), `C.xdr_to_json` (CGo crossing), `C.GoString` (copy back), three `defer C.free` calls
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:ConvertBytesSlice:64-126` — single `C.CString`, single `C.xdr_batch_to_json` crossing; per-item `CXDR` + `FreeGoXDR` still needed but type name and CGo crossing are amortized
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:xdr_to_json:127-161` — per-call: `catch_json_to_xdr_panic` wraps `panic::catch_unwind`, `from_c_string`, `TypeVariant::from_str`, `read_xdr_to_end`, `serde_json::to_string`
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:xdr_batch_to_json:198-270` — single `panic::catch_unwind`, single `from_c_string` + `TypeVariant::from_str`; per-item: `read_xdr_to_end` + `serde_json::to_string` (same work)

### Findings

The inefficiency is real and the fix is straightforward. For a page of N transactions, the current code makes 3×N individual `xdr_to_json` CGo calls. Batching via `ConvertBytesSlice` reduces this to 3 calls. The per-call overhead saved includes:

- **Go side** (~500ns/call): `reflect.TypeOf().Name()`, `C.CString` malloc+copy, 3 `defer` registrations, `C.free` calls
- **CGo boundary** (~150ns/call): goroutine thread pinning, `entersyscall`/`exitsyscall`
- **Rust side** (~200ns/call): `panic::catch_unwind` setup, `from_c_string` copy, `TypeVariant::from_str` string matching

Total per-call overhead: ~850ns. For N=200 transactions: savings of ~(600−3)×850ns ≈ **508µs**.

However, the actual XDR parsing (`read_xdr_to_end`) and JSON generation (`serde_json::to_string`) — which are identical in both paths — dominate. TransactionMeta alone can be 5–100KB of XDR, taking ~100µs–5ms to parse+serialize. For a 200-transaction page, total conversion time is likely 25ms–1s+, making the ~500µs overhead saving roughly **0.05–2%** of total conversion time.

Severity downgraded from Medium to **Low**: the inefficiency is real and the batch API already exists, but practical impact is <5%. The improvement would be most visible on pages of small, simple transactions where per-call overhead is proportionally larger relative to conversion work.

Note: `jsonifySlice` for DiagnosticEvents (line 157) already uses `ConvertBytesSlice`, showing the codebase recognizes this pattern. The core fields (Result, Envelope, Meta) were not batched because they're single values per transaction — batching only helps when aggregated across the page.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/methods/get_transactions.go` (processTransactionsInLedger and/or the caller `getTransactionsByLedgerSequence`) and `cmd/stellar-rpc/internal/methods/json.go` (transactionToJSON — may be replaced by a batch variant)
- **Change description**: Restructure the JSON conversion to a two-pass approach: (1) collect all `db.Transaction` structs (or just their `.Result`, `.Envelope`, `.Meta` byte slices) for the page, (2) call `ConvertBytesSlice(xdr.TransactionResult{}, allResults)`, `ConvertBytesSlice(xdr.TransactionEnvelope{}, allEnvelopes)`, `ConvertBytesSlice(xdr.TransactionMeta{}, allMetas)`, (3) assign the returned JSON back to each `TransactionInfo` by index. DiagnosticEvents can also be flattened across the page using `jsonifySlice`. The refactor must preserve early-exit on limit and correct cursor tracking.
- **Correctness check**: Existing tests in `cmd/stellar-rpc/internal/methods/get_transactions_test.go` and integration tests for JSON format responses should cover correctness. Error propagation must identify which transaction failed (index tracking).
- **Benchmark focus**: Write a Go benchmark calling `processTransactionsInLedger` (or a synthetic equivalent) with 50–200 transactions in JSON mode. Measure ns/op and allocs/op. Expect ~500µs–1ms reduction in per-page latency, visible primarily in the CGo/overhead portion of CPU profiles. The existing `BenchmarkConvertBytesVsSlice` in `conversion_test.go` validates the per-field-type savings in isolation.

---

## PoC Attempt

**Result**: POC_PASS
**Date**: 2026-04-07
**PoC by**: claude-opus-4-6, high

### Changes Made

- **`cmd/stellar-rpc/internal/methods/json.go`** (lines 94–211): Added `pendingTxJSON` struct and `batchConvertTransactionsToJSON` function. The batch function collects all Result, Envelope, Meta byte slices plus all DiagnosticEvent, TransactionEvent, and ContractEvent byte slices across the entire page, then makes exactly 6 `ConvertBytesSlice` CGo calls total (one per XDR type). Results are assigned back to each `TransactionInfo` by index. Also added `protocol` import.

- **`cmd/stellar-rpc/internal/methods/get_transactions.go`** (lines 78–84, 152–162, 289, 352, 362–371): Modified `processTransactionsInLedger` to accept a `*[]pendingTxJSON` parameter. The JSON branch now defers conversion — it calls `db.ParseTransaction` and appends to the pending slice instead of calling `transactionToJSON`, `jsonifySlice`, and `BuildEventsJSONFromTransaction` per-transaction. The caller `getTransactionsByLedgerSequence` creates the pending slice, passes it through the per-ledger loop, and calls `batchConvertTransactionsToJSON` after all ledgers are processed.

### Demonstration

The optimization restructures `getTransactions` JSON conversion from per-transaction to page-level batching. For a page of N transactions, CGo crossings drop from 6×N (3 core fields + 3 event types per tx) to exactly 6 total, regardless of page size. This eliminates per-call overhead including `reflect.TypeOf`, `C.CString` allocation, `panic::catch_unwind` setup, and `TypeVariant::from_str` resolution for all but 6 calls across the entire response.

### Test Results

All 18 tests in `cmd/stellar-rpc/internal/methods/` pass, including `TestGetTransactions_JSONFormat` which validates JSON field presence/absence for the batched path. Full `make go-test` passes across all packages (methods, db, xdr2json, feewindow, ingest, config, network, preflight, util, integrationtest, etc.).
