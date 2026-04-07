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
