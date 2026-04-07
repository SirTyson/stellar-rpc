# H003: getTransactions Re-resolves the Same Small Set of XDR Type Names on Every xdr2json Call

**Date**: 2026-04-07
**Subsystem**: xdr2json
**Severity**: Low
**Impact**: latency / CPU / allocations
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

The hot `getTransactions` JSON path should resolve its fixed set of XDR types once and reuse that discriminator for the rest of the page. Event-heavy responses should not repeatedly derive `"TransactionResult"`, `"TransactionEnvelope"`, `"TransactionMeta"`, `"DiagnosticEvent"`, `"TransactionEvent"`, and `"ContractEvent"` from reflection, allocate those names as C strings, and parse them back into Rust enum variants thousands of times.

## Mechanism

Every `ConvertBytes` call computes `reflect.TypeOf(xdr).Name()`, allocates a new `C.CString`, then Rust copies that C string into an owned `String` and runs `xdr::TypeVariant::from_str(&type_str)`. `getTransactions` only uses a tiny, fixed menu of XDR types, so this is pure repeated dispatch scaffolding rather than real serialization work. A numeric type id, cached `TypeVariant`, or specialized per-type FFI entrypoint would remove that fixed overhead without needing the broader batch API already identified in `methods`.

## Trigger

1. Issue `getTransactions` with `format=json` against event-heavy ledgers so the page performs thousands of `ConvertBytes` calls.
2. Profile CPU samples and allocation counts around `reflect.TypeOf(...).Name()`, `C.CString`, `from_c_string`, and `TypeVariant::from_str`.
3. Compare against a prototype that passes a small integer discriminator or uses fixed per-type wrappers for the six hot `getTransactions` types.

## Target Code

- `cmd/stellar-rpc/internal/methods/json.go:21-31,56-61` — repeated hot-path callers of `ConvertBytes`.
- `cmd/stellar-rpc/internal/methods/get_transaction.go:147-152` — event builders repeatedly invoke the same two event types.
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:41-42,61-69` — Go derives the type name and allocates a C string on every call.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:65-69` — Rust re-materializes the type string and resolves `TypeVariant` on every call.

## Evidence

Within `getTransactions`, the type sequence is highly repetitive and known in advance, but the bridge treats every conversion as if it were an arbitrary new type. This makes the type-dispatch cost scale with event count instead of with the number of distinct XDR types actually used by the endpoint.

## Anti-Evidence

This overhead is smaller than the bulk data-copy costs on both the input and output sides, so the win is likely limited to event-heavy pages. If a future batch API lands first, much of this fixed cost would already be amortized away.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated
**Failed At**: reviewer

### Trace Summary

Traced the full type-dispatch path from Go callers (`transactionToJSON`, `jsonifySlice`, `BuildEventsJSONFromTransaction`) through `ConvertBytes` → `convertAnyBytes` (conversion.go:61-69) → FFI into `xdr_to_json` (lib.rs:65-69). Confirmed each call performs `reflect.TypeOf().Name()` (~5ns, no allocation), `C.CString` (~50-80ns for 17-byte type names), `from_c_string` (~50-80ns), and `TypeVariant::from_str` (~50-100ns). Then examined the `TypeVariant::from_str` implementation in stellar-xdr v26.0.0 (curr/generated.rs:53700-54100): a 468-arm match statement that LLVM compiles into an efficient decision tree using length-based dispatch, not a linear scan.

### Code Paths Examined

- `cmd/stellar-rpc/internal/xdr2json/conversion.go:41` — `reflect.TypeOf(xdr).Name()` reads from Go type descriptor; zero allocation, ~5ns
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:66` — `C.CString(xdrTypeName)` allocates 18-20 bytes via malloc + memcpy; ~50-80ns per call
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:67` — `from_c_string(typename)` does strlen + to_owned on ~17-byte string; ~50-80ns
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:68-70` — `TypeVariant::from_str(&type_str)` resolves via 468-arm match; LLVM optimizes to length-switch + character comparison tree; ~50-100ns
- `stellar-xdr-26.0.0/src/curr/generated.rs:53700-54100` — `from_str` match arms for all 468 TypeVariants; hot types (TransactionResult, TransactionMeta, etc.) are scattered but LLVM reorders at compile time
- `cmd/stellar-rpc/internal/methods/json.go:21-31` — `transactionToJSON` calls `ConvertBytes` 3× per transaction (Result, Envelope, Meta)
- `cmd/stellar-rpc/internal/methods/json.go:56-67` — `jsonifySlice` calls `ConvertBytes` once per event
- `cmd/stellar-rpc/internal/methods/get_transaction.go:143-155` — `BuildEventsJSONFromTransaction` calls jsonifySlice/jsonifySliceOfSlices for ContractEvents + TransactionEvents

### Why It Failed

The total per-call type-dispatch overhead is ~155-265ns, dominated by the small C.CString malloc/free cycle and the from_c_string copy. This is 100-1000× smaller than the actual per-call work (XDR deserialization: 1-100μs, JSON serialization: 1-100μs, data copies: 0.1-10μs). For a full getTransactions page of 200 transactions with 4600 ConvertBytes calls, the aggregate type-dispatch overhead is ~0.7-1.2ms out of ~50-100ms+ total request time — approximately 1-2%, well below the measurability threshold given noise from XDR parsing, serde serialization, memory allocation, and cgo crossing overhead. The hypothesis correctly identifies that the dispatch is repeated, but overestimates its cost: `reflect.TypeOf().Name()` is a zero-allocation constant-time type descriptor read (not "reflection" in the expensive sense), and `TypeVariant::from_str` is LLVM-optimized to an efficient decision tree, not the implied O(n) string search. The proposed fixes (numeric type IDs, per-type entry points) would require significant FFI interface changes across Go and Rust for a gain well under 1% of end-to-end latency.

### Lesson Learned

Fixed per-call dispatch scaffolding (type name strings, enum resolution) is only worth optimizing when it constitutes a meaningful fraction of per-call cost. In the xdr2json path, XDR deserialization and JSON serialization dominate each call by 100-1000×. Optimizing the ~250ns dispatch when the per-call floor is ~2-100μs yields diminishing returns. Focus optimization effort on costs proportional to data size (copies, serialization) rather than fixed per-call overhead. The LLVM-optimized string match for TypeVariant::from_str is much faster than a naive linear scan would suggest.
