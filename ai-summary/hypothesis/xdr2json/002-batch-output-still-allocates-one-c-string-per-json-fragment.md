# H002: xdr_batch_to_json Still Allocates One C String Per JSON Fragment

**Date**: 2026-04-07
**Subsystem**: xdr2json
**Severity**: Low
**Impact**: latency / CPU / allocations
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

The batched xdr2json path should return batched JSON in a representation that avoids one Rust heap object, one Rust free, and one Go `strlen`-style walk per converted item. Event-heavy `getTransactions` responses should not reduce CGo crossings only to immediately recreate thousands of standalone C strings on the way back out.

## Mechanism

Inside `xdr_batch_to_json`, every successful item executes `string_to_c(serde_json::to_string(&t).unwrap())`, so each JSON fragment becomes its own heap-allocated NUL-terminated string. `ConvertBytesSlice` then calls `C.GoString(cResults[i].json)` for every result, and `free_batch_conversion_result` loops back over the entire array calling `free_c_string` per item. A batch result arena with per-item lengths or offsets would preserve the batching win while removing the per-item CString allocation/free cycle and the per-item `C.GoString` length scan that currently scale with event count.

## Trigger

1. Issue `getTransactions` with `format=json` against a page containing thousands of small-to-medium event JSON fragments.
2. Capture allocation profiles around `xdr_batch_to_json`, `free_batch_conversion_result`, and `C.GoString`.
3. Compare against a prototype that returns one owned byte arena plus per-item `(offset,len)` metadata instead of `conversion_result_t{json:*char}` entries.

## Target Code

- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:xdr_batch_to_json:218-239` — each successful result currently becomes a standalone `CString`.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:free_batch_conversion_result:278-299` — batch teardown frees every result string individually.
- `cmd/stellar-rpc/lib/xdr2json.h:batch_conversion_result_t:8-12` — the C ABI currently exposes only per-item string pointers.
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:112-123` — Go walks each result with `C.GoString` and re-materializes `json.RawMessage`.
- `cmd/stellar-rpc/internal/methods/json.go:jsonifySlice/jsonifySliceOfSlices:56-90` — `getTransactions` event families all consume this batch result shape.

## Evidence

The current batch API amortizes type resolution and the FFI boundary, but it still allocates and frees output storage one JSON fragment at a time. `getTransactions` is the endpoint most likely to stress that shape because it already batches event conversion and can return very large counts of contract, transaction, and diagnostic events in one page.

## Anti-Evidence

Go still needs at least one copy into GC-managed memory before the response escapes cgo, so this is not a zero-copy design. Large `TransactionMeta` JSON blobs may still be dominated by `serde_json` itself rather than by the per-item output allocator churn.
