# H004: JSON page batching still pays one C allocation and memcpy per XDR field

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Medium
**Impact**: CPU / allocation / CGo overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When `getTransactions` batches JSON conversion across a page, the remaining FFI overhead should be close to one crossing per field type, not one C heap allocation and memcpy per individual XDR buffer. Event-heavy JSON pages should not spend a large fraction of time copying already-materialized Go byte slices into temporary C buffers.

## Mechanism

Even on the batched path, `ConvertBytesSlice()` still loops over every input buffer and calls `CXDR(field)`, which uses `C.CBytes` to allocate and copy that item into C heap memory before the Rust converter runs. For a large JSON page this means hundreds or thousands of `malloc`/`memcpy`/`free` operations survive after batching; a zero-copy descriptor API or a single arena-with-offsets batch format could preserve the low CGo crossing count while removing the per-item C heap churn.

## Trigger

Request `getTransactions` in `json` format over event-heavy Soroban ledgers with a large limit. Profiles should still show substantial time and allocations under `CXDR`, `C.CBytes`, and `FreeGoXDR` even after the page-level batching hypotheses for core fields and events are applied.

## Target Code

- `cmd/stellar-rpc/internal/methods/json.go:12-36` — core field JSON conversion routes through `ConvertBytes`
- `cmd/stellar-rpc/internal/methods/json.go:56-90` — batched event conversion routes through `ConvertBytesSlice`
- `cmd/stellar-rpc/internal/methods/get_transactions.go:159-180` — JSON path invokes these converters for every returned transaction
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:61-126` — `ConvertBytesSlice()` still performs per-item `CXDR()` setup and teardown
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:149-159` — `CXDR()` uses `C.CBytes`, forcing a separate C heap copy per XDR buffer

## Evidence

`transactionToJSON()` and the slice helpers are the only path from `getTransactions` JSON mode into Rust (`cmd/stellar-rpc/internal/methods/json.go:12-36,56-90`). `ConvertBytesSlice()` reduces the number of CGo crossings but still builds a `[]C.xdr_t` by calling `CXDR(field)` for each non-empty element (`cmd/stellar-rpc/internal/xdr2json/conversion.go:74-103`), and `CXDR()` is a thin wrapper over `C.CBytes` (`cmd/stellar-rpc/internal/xdr2json/conversion.go:149-154`). `getTransactions` invokes these helpers for result, envelope, meta, diagnostic events, transaction events, and contract events on every JSON page item (`cmd/stellar-rpc/internal/methods/get_transactions.go:159-180`).

## Anti-Evidence

If Rust-side XDR parsing and JSON serialization dominate the page, copy elimination alone will not remove all of the latency. Any new FFI shape must still surface per-item conversion errors clearly and stay within cgo pointer-safety rules.
