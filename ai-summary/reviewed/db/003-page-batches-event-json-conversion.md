# H003: Event-heavy JSON pages still cross the FFI boundary once per transaction

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Medium
**Impact**: CPU / CGo / allocation overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

For JSON-format `getTransactions`, diagnostic events, transaction events, and contract events should be converted in page-wide batches. An event-heavy page should pay a small constant number of FFI conversions per event type, not one conversion round per returned transaction.

## Mechanism

`processTransactionsInLedger()` calls `jsonifySlice()` for diagnostic events and then `BuildEventsJSONFromTransaction()` for every transaction. `BuildEventsJSONFromTransaction()` batches contract events only within a single transaction, then repeats the same conversion pattern for the next transaction, so a page with `N` event-bearing transactions still performs O(N) batch conversions and repeated CGo setup. Because `ConvertBytesSlice()` already preserves ordering for homogeneous slices, the handler can flatten page-wide diagnostic/transaction/contract event byte buffers, convert them once per event type, and then split the results back by transaction and operation offsets.

## Trigger

Call `getTransactions` with `xdrFormat=json` over ledgers containing many Soroban transactions with diagnostic, transaction, and contract events. A profile should show repeated `xdr_batch_to_json` work originating from each transaction's event conversion helpers rather than a few large batched conversions for the page.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:157-170` — per-transaction diagnostic/event JSON conversion in the hot loop
- `cmd/stellar-rpc/internal/methods/get_transaction.go:BuildEventsJSONFromTransaction:143-155` — event conversion helper called separately for each transaction
- `cmd/stellar-rpc/internal/methods/json.go:jsonifySlice/jsonifySliceOfSlices:56-90` — batching is currently limited to a single transaction's inner slices
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:ConvertBytesSlice:61-126` — batch converter already supports the needed slice semantics

## Evidence

The JSON branch in `processTransactionsInLedger()` converts diagnostic events via `jsonifySlice()` and then calls `BuildEventsJSONFromTransaction()` for every transaction (`cmd/stellar-rpc/internal/methods/get_transactions.go:157-170`). `BuildEventsJSONFromTransaction()` in turn invokes `jsonifySliceOfSlices()` for contract events and `jsonifySlice()` for transaction events, but only for that one transaction (`cmd/stellar-rpc/internal/methods/get_transaction.go:143-155`). `jsonifySliceOfSlices()` does flatten inner operation slices, yet it rebuilds that flattened buffer anew for each transaction instead of across the whole page (`cmd/stellar-rpc/internal/methods/json.go:60-90`). The underlying converter is already optimized for large homogeneous batches (`cmd/stellar-rpc/internal/xdr2json/conversion.go:61-126`), so the remaining inefficiency is the page-level call pattern, not missing lower-level support.

## Anti-Evidence

Classic transactions with no events will see little or no benefit from this change. Any page-wide batching fix must preserve the exact nested per-transaction / per-operation grouping and the empty-array behavior expected by the RPC protocol.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the `getTransactions` JSON path from `processTransactionsInLedger` through the per-transaction event conversion calls. Confirmed that for each transaction in the page loop, three separate `ConvertBytesSlice` CGo crossings occur: one for DiagnosticEvents (line 157), one for TransactionEvents, and one for ContractEvents (lines 170→143-155→56-91). Each crossing pays ~850ns of per-batch overhead (reflect, C.CString malloc, CGo boundary, Rust panic::catch_unwind + TypeVariant::from_str). The `jsonifySliceOfSlices` function already demonstrates the flatten-convert-split pattern within a transaction, proving the page-level extension is architecturally sound.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:112-199` — for each transaction in the hot loop, line 157 calls `jsonifySlice(DiagnosticEvent, tx.Events)` and line 170 calls `BuildEventsJSONFromTransaction(tx)`, producing 3 `ConvertBytesSlice` CGo crossings per transaction
- `cmd/stellar-rpc/internal/methods/get_transaction.go:BuildEventsJSONFromTransaction:143-155` — calls `jsonifySliceOfSlices(ContractEvent, tx.ContractEvents)` then `jsonifySlice(TransactionEvent, tx.TransactionEvents)`, each scoped to a single transaction
- `cmd/stellar-rpc/internal/methods/json.go:jsonifySlice:56-58` — thin wrapper over `ConvertBytesSlice`; called once per transaction per event type
- `cmd/stellar-rpc/internal/methods/json.go:jsonifySliceOfSlices:62-91` — flattens inner slices (per-operation) into one batch, but only within a single transaction; the flatten-convert-split pattern is already proven here
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:ConvertBytesSlice:64-126` — per call: `reflect.TypeOf().Name()`, `C.CString` malloc+copy, `C.xdr_batch_to_json` CGo crossing, cleanup; the per-item XDR→JSON work is identical whether batched per-transaction or per-page
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:xdr_batch_to_json:198-270` — per call: `panic::catch_unwind`, `from_c_string`, `TypeVariant::from_str`; per item: `read_xdr_to_end` + `serde_json::to_string` (unchanged by batching)

### Findings

The inefficiency is real. For a page of N event-bearing transactions, the current code makes 3×N `ConvertBytesSlice` CGo calls for events. Page-level batching reduces this to 3 calls. The per-call overhead saved:

- **Go side** (~500ns/call): `reflect.TypeOf().Name()`, `C.CString` malloc+copy, `defer` registrations and `C.free` calls
- **CGo boundary** (~150ns/call): goroutine thread pinning, `entersyscall`/`exitsyscall`
- **Rust side** (~200ns/call): `panic::catch_unwind` setup, `from_c_string` copy, `TypeVariant::from_str` string matching

Total per-call overhead: ~850ns. For N=200 transactions: savings of ~(600−3)×850ns ≈ **508µs**.

However, the actual XDR parsing (`read_xdr_to_end`) and JSON generation (`serde_json::to_string`) — identical in both paths — dominate per-event cost. Individual events (DiagnosticEvent, ContractEvent, TransactionEvent) are typically 100B–2KB of XDR, taking ~5–50µs each to parse+serialize. For a 200-transaction page with an average of 5 events/tx (1000 total events), total event conversion time is ~5–50ms, making the ~500µs overhead saving roughly **1–10%** of event conversion time, but only **0.05–2%** of total page conversion time (which includes the much larger TransactionMeta).

Severity downgraded from Medium to **Low**: the inefficiency is real and the batch API already exists, but practical impact is <5% of total getTransactions latency. The improvement is most visible on pages dominated by many small Soroban transactions with numerous events.

**Relationship to H002**: This finding is complementary to H002 (page-level batching of core fields Result/Envelope/Meta). H002 targets per-transaction `ConvertBytes` calls; H003 targets per-transaction `ConvertBytesSlice` calls for events. Both optimizations could be implemented together for cumulative savings of ~1ms on a 200-transaction page. H002's PoC Guidance mentions DiagnosticEvents as an extension, but does not cover TransactionEvents or ContractEvents.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/methods/get_transactions.go` (processTransactionsInLedger) — restructure to two-pass: (1) collect all `db.Transaction` structs for the page, (2) batch-convert events across all transactions. Also `cmd/stellar-rpc/internal/methods/json.go` — add a page-level event batching function analogous to `jsonifySliceOfSlices` but operating across transactions.
- **Change description**: After collecting all transactions for the page (or up to the limit), flatten all DiagnosticEvents byte slices across transactions into one `[][]byte`, all TransactionEvents into another, and all ContractEvents into a third. Call `ConvertBytesSlice` once per event type (3 calls total). Use offset tracking (similar to `jsonifySliceOfSlices` lines 84-88) to split results back into per-transaction arrays. For ContractEvents, track both per-transaction and per-operation offsets to reconstruct `[][]json.RawMessage` per transaction. Must preserve early-exit on limit and correct cursor tracking — the two-pass approach should collect only up to `limit` transactions before converting.
- **Correctness check**: Existing tests in `cmd/stellar-rpc/internal/methods/get_transactions_test.go` and integration tests for JSON format responses cover correctness. Ensure empty event arrays are preserved (transactions with no events must still produce `[]` not `null`). The `jsonifySliceOfSlices` function's empty-slice handling (line 66) provides the template.
- **Benchmark focus**: Write a Go benchmark with 50–200 Soroban transactions each having 5–10 events in JSON mode. Measure ns/op and allocs/op. Expect ~500µs reduction in per-page latency, visible primarily in CGo overhead. Best combined with H002's core field batching for cumulative ~1ms savings. The existing `BenchmarkConvertBytesVsSlice` in `conversion_test.go` validates per-field-type savings in isolation.
