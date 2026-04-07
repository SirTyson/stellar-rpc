# H002: xdr2json Copies JSON Output Through a C String and a Go String Before Returning RawMessage

**Date**: 2026-04-07
**Subsystem**: xdr2json
**Severity**: Medium
**Impact**: latency / CPU / allocations / memory bandwidth
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once Rust has serialized an XDR object to JSON, the result should cross the FFI boundary into Go with at most one payload copy into the final `json.RawMessage`. Large `getTransactions` JSON pages should not re-copy every serialized result, envelope, meta blob, and event body through multiple transient string representations on the success path.

## Mechanism

`xdr_to_json` builds a Rust `String` with `serde_json::to_string`, then `string_to_c` allocates a second null-terminated copy for the FFI result. On the Go side, `C.GoString(result.json)` copies that buffer again into a Go `string`, and `json.RawMessage(jsonStr)` copies the bytes a fourth time into the final slice-backed response field. Because `TransactionMeta` JSON and event JSON are often larger than their XDR inputs, this extra output copying can consume measurable CPU and allocation budget even if the call count stays the same.

## Trigger

1. Issue `getTransactions` with `format=json` against ledgers with large `resultMeta`, `diagnosticEvents`, and contract-event payloads.
2. Capture allocation profiles on the Rust-to-Go transfer path, especially around `serde_json::to_string`, `string_to_c`, `C.GoString`, and `json.RawMessage(...)`.
3. Compare against a prototype that returns `(ptr,len)` JSON bytes from Rust and materializes them with a single `C.GoBytes`/owned-buffer transfer into Go.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:164-185` — `getTransactions` stores many separate JSON fragments returned by `xdr2json`.
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:69-79` — Go copies the JSON result into a `string` and then into `json.RawMessage`.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:80-89` — Rust serializes to `String` and then allocates C strings for both `json` and `error`.
- `cmd/stellar-rpc/lib/ffi/src/lib.rs:29-46` — `safe_cstring`/`string_to_c` scan and copy the full JSON payload into a new `CString`.
- `cmd/stellar-rpc/lib/xdr2json.h:3-11` — the FFI result shape exposes only null-terminated strings, forcing the string bridge.

## Evidence

The current API returns JSON as a C string even though Go ultimately wants a byte slice (`json.RawMessage`). That design forces extra success-path allocations and copies for every converted fragment, and `getTransactions` exercises the path repeatedly on the largest JSON-producing types in the codebase.

## Anti-Evidence

At least one copy into Go-owned memory is still required before the response escapes the cgo call, so this is not a true zero-copy output path. Small JSON fragments will see a smaller gain than large metas or event batches.
