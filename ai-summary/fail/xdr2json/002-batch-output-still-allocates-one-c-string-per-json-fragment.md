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

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: FAIL — substantially equivalent to reviewed H002 (json-output-copied-through-string-bridge)
**Failed At**: reviewer

### Trace Summary

Traced the full batch output path from `xdr_batch_to_json` (lib.rs:218-239) through the FFI boundary into `ConvertBytesSlice` (conversion.go:112-123). Confirmed each item does `string_to_c(serde_json::to_string(&t).unwrap())` creating a per-item CString (NUL scan via `safe_cstring` + heap allocation), followed by per-item `C.GoString` (strlen + malloc + memcpy) and `json.RawMessage(jsonStr)` (string→[]byte copy) on the Go side, then per-item `free_c_string` in `free_batch_conversion_result`. However, the already-reviewed H002 ("json-output-copied-through-string-bridge") addresses this same fundamental problem via its `conversion_result_t` struct change, which applies to both single-item and batch paths.

### Code Paths Examined

- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:228` — `string_to_c(serde_json::to_string(&t).unwrap())` — per-item CString allocation; calls `safe_cstring` which does O(n) NUL scan + CString heap allocation
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:287-289` — `free_c_string(r.json); free_c_string(r.error)` — per-item deallocation loop
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:115` — `C.GoString(cResults[i].json)` — per-item strlen O(n) + malloc + memcpy
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:122` — `json.RawMessage(jsonStr)` — per-item string→[]byte copy
- `cmd/stellar-rpc/lib/ffi/src/lib.rs:29-47` — `safe_cstring`/`string_to_c` — the NUL scan and CString allocation machinery
- `cmd/stellar-rpc/lib/xdr2json.h:3-6` — `conversion_result_t` struct shared by both single-item and batch paths

### Why It Failed

**Substantially equivalent to reviewed H002 (json-output-copied-through-string-bridge).** The reviewed H002 proposes changing `conversion_result_t` from `const char* json` to `const uint8_t* json_ptr; size_t json_len;`, which eliminates the NUL scan in `safe_cstring`, the `strlen` in `C.GoString`, and the `string→[]byte` copy in Go. Since `batch_conversion_result_t` contains `conversion_result_t* results`, the struct change applies to the batch path automatically. After H002's fix, the batch path's per-item waste reduces from ~200-400ns (CString alloc + NUL scan + strlen + string→[]byte copy + free_c_string) to ~20-50ns (raw byte buffer alloc + free).

The **additional** arena consolidation proposed by this hypothesis (one contiguous buffer + offset/length metadata instead of per-item byte buffers) would save only the remaining allocator overhead: ~20-50ns × N items. For a 4000-event page, that's ~80-200μs — well under 0.5% of a 50-200ms request. This marginal gain does not justify the complexity of a new arena-based FFI ABI (different `BatchConversionResult` struct, `serde_json::to_writer` into a shared buffer, partial-write error handling, new Go-side extraction code).

Furthermore, the Go side cannot avoid per-item copies regardless of the Rust-side representation: each `json.RawMessage` must be a separate Go-managed `[]byte` slice, so the Go side must still perform N `C.GoBytes` or equivalent operations to extract individual JSON fragments from any arena.

### Lesson Learned

When evaluating batch-path optimizations, check whether a fix to shared data structures (like `conversion_result_t`) from a related hypothesis already covers the batch path automatically. The most impactful waste in the output bridge (NUL scan, strlen, string→[]byte copy) is already addressed by the `(ptr, len)` approach from H002. Arena consolidation of per-item allocations is a second-order optimization that yields diminishing returns when the per-item allocation cost is already reduced to a raw byte buffer alloc/free cycle (~20-50ns).
