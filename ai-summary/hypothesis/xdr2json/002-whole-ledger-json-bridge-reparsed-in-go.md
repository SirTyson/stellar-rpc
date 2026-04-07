# H002: `LCMTransactionsToJSON` Still Round-Trips Through a Whole-Ledger JSON Document in Go

**Date**: 2026-04-07
**Subsystem**: xdr2json
**Severity**: Medium
**Impact**: latency / CPU / allocations / GC
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once Rust has extracted per-transaction JSON fragments from a raw
`LedgerCloseMeta`, Go should receive those fragments in a structured FFI result
that it can assign directly into `protocol.TransactionInfo`. The hot path should
not serialize a temporary ledger-wide JSON array and then immediately parse that
document back into Go structs before the final RPC response is marshaled.

## Mechanism

`lcm_transactions_to_json()` produces one large JSON string for the entire ledger,
and `LCMTransactionsToJSON()` copies that buffer into Go and runs
`json.Unmarshal(jsonBytes, &txns)`. That outer array/object serialization is
staging-only work: Go ultimately re-serializes the response later, and the helper
only needs the raw per-field fragments plus a few scalars. Replacing the
whole-document bridge with an FFI result shaped like `[]LCMTransactionJSON` (or
parallel fragment buffers plus scalar metadata) would remove one full JSON encode
of the ledger container and one full JSON decode of that container in Go.

## Trigger

1. Issue `getTransactions` with `format=json` for 100-200 transactions with large
   `resultMetaJson` and event payloads.
2. Profile CPU and allocations around `lcm_transactions_to_json`,
   `C.GoBytes`, and `encoding/json.Unmarshal`.
3. Compare against a prototype that returns per-transaction structured results
   directly over FFI instead of a ledger-wide JSON array string.

## Target Code

- `cmd/stellar-rpc/internal/xdr2json/conversion.go:45-79` — Go copies the helper output and reparses it with `json.Unmarshal`.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:351-377` — `lcm_transactions_to_json()` returns a single `ConversionResult` containing one JSON blob.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:475-556` — Rust serializes the ledger into one array string via `serde_json::to_string(&result_array)`.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:281-323` — the parsed Go structs are then rewrapped into `protocol.TransactionInfo`.

## Evidence

The Go wrapper does `jsonBytes := C.GoBytes(...)` and then `json.Unmarshal(...)`
for every selected ledger. The Rust side serializes `"hash"`, `"application_order"`,
`"result"`, `"meta"`, `"envelope"`, and event arrays into a temporary outer JSON
document even though Go only needs to preserve those fragments for the final
response shape.

## Anti-Evidence

`json.RawMessage` means Go does not deeply decode the nested `result`, `meta`,
`envelope`, and event bodies, so this is not a full second parse of every inner
JSON fragment. The endpoint still needs at least one owned copy into Go-managed
memory before the response escapes cgo.
