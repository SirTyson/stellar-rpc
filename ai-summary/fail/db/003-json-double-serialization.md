# H003: JSON pages serialize each transaction to XDR bytes before the JSON converter parses them again

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: High
**Impact**: CPU / CGo / allocation overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When `getTransactions` is requested in `json` format, the handler should convert each transaction field to JSON with a single serialization step. It should not first marshal Go XDR structs into byte slices only to hand those byte slices back across the FFI boundary for another parse.

## Mechanism

`processTransactionsInLedger()` always calls `db.ParseTransaction()`, and `ParseTransaction()` eagerly `MarshalBinary()`s the result, meta, envelope, diagnostic events, transaction events, and contract events into `[]byte`. The JSON branch then immediately feeds those byte slices into `xdr2json.ConvertBytes()` / `ConvertBytesSlice()`, so each returned transaction pays two serialization passes plus multiple buffer allocations and C memory copies before the final JSON reaches the client.

## Trigger

Call `getTransactions` with `xdrFormat=json` on ledgers containing Soroban transactions with diagnostic, transaction, and contract events. A CPU/alloc profile should show time in both `MarshalBinary()` inside `ParseTransaction()` and in `xdr2json` conversion for the same fields.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:125-180` — unconditional call to `db.ParseTransaction()` before the JSON/XDR format split
- `cmd/stellar-rpc/internal/db/transaction.go:ParseTransaction:238-321` — eager XDR marshaling of all transaction fields and events
- `cmd/stellar-rpc/internal/methods/json.go:transactionToJSON:12-36` — re-parses result/meta/envelope bytes through `xdr2json`
- `cmd/stellar-rpc/internal/methods/json.go:jsonifySlice/jsonifySliceOfSlices:56-90` — batches event byte slices through the FFI converter
- `cmd/stellar-rpc/internal/methods/get_transaction.go:BuildEventsJSONFromTransaction:142-155` — repeats the event-byte-to-JSON conversion pattern

## Evidence

The format branch happens only after `db.ParseTransaction()` returns a fully byte-encoded `db.Transaction` (`cmd/stellar-rpc/internal/methods/get_transactions.go:125-180`). `ParseTransaction()` serializes every field and event to `[]byte` regardless of requested format (`cmd/stellar-rpc/internal/db/transaction.go:258-319`), while `transactionToJSON()` and `BuildEventsJSONFromTransaction()` immediately send those same bytes into `xdr2json` (`cmd/stellar-rpc/internal/methods/json.go:21-36,56-90` and `cmd/stellar-rpc/internal/methods/get_transaction.go:143-155`).

## Anti-Evidence

This does not affect the default XDR response path, which already needs serialized bytes for base64 output. Any fix must keep the exact JSON field ordering and formatting expected by the existing RPC protocol and tests.

---

## Review

**Verdict**: NEEDS_REFINEMENT
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Failed At**: reviewer

### What's Wrong

The hypothesis correctly identifies that for JSON-format requests, data undergoes an XDR round-trip: DB blob → full Go struct deserialization (`BatchGetLedgerMetas`) → `MarshalBinary()` back to XDR bytes (`ParseTransaction`) → C memory copy (`CXDR`) → Rust XDR parse → JSON serialize. However, the proposed mechanism — "skip MarshalBinary before the format branch" — does not produce meaningful savings because:

1. **MarshalBinary is required by the FFI architecture.** `xdr2json.ConvertBytes` and `ConvertBytesSlice` accept `[]byte` (serialized XDR). The alternative `ConvertInterface` also calls `MarshalBinary` internally (xdr2json/conversion.go:53), so moving the marshal call changes nothing about total CPU work.

2. **The batch optimization would be lost.** `ConvertBytesSlice` amortizes CGo crossing overhead across all events in a single call (conversion.go:92). Replacing it with per-event `ConvertInterface` calls would increase CGo crossings and likely hurt performance for event-heavy transactions.

3. **Both format paths need the bytes.** The XDR/base64 path needs `[]byte` for `base64.StdEncoding.EncodeToString`. The JSON path needs `[]byte` for the FFI. `ParseTransaction` is doing exactly the work both paths require — there is no unconditionally wasted serialization.

The "two serialization passes" framing is misleading. There is one XDR serialization (Go `MarshalBinary`) and one format conversion (Rust XDR→JSON). Both are necessary steps for the current FFI-based architecture. The real waste is the XDR *round-trip* (deserialize from DB blob, then re-serialize for FFI), not a "double serialization."

### Alternative Angle

The genuine optimization opportunity is to **bypass the Rust FFI entirely for the JSON path**, eliminating both the `MarshalBinary` and the CGo/Rust processing. Two approaches:

1. **Go-native XDR-to-JSON**: If the `go-stellar-sdk` XDR types support `json.Marshaler` (or can be made to produce wire-compatible JSON), the JSON path could marshal Go structs directly to JSON without any FFI crossing. This would eliminate: MarshalBinary (Go XDR serialize), C.CBytes (Go→C copy), Rust XDR parse, CGo return copy. This is a significant change that requires verifying JSON output compatibility with the Rust library's output format.

2. **Raw-blob-to-JSON FFI**: `BatchGetLedgers` already retains the raw LCM bytes (`LedgerMetadataChunk.Lcm`). A new Rust FFI function could accept the entire raw LCM blob and produce per-transaction JSON directly, avoiding Go-side full deserialization + re-serialization entirely. This keeps the Rust JSON format guarantees but requires new FFI surface area.

### Additional Code Paths

- `cmd/stellar-rpc/internal/db/ledger.go:BatchGetLedgers:74-115` — already provides raw LCM bytes alongside partial header decode; could serve as the entry point for a raw-blob-to-JSON approach
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:ConvertInterface:51-59` — demonstrates that even the "pass Go struct" path still calls MarshalBinary internally
- Go stellar-sdk XDR type definitions — need to check whether generated types implement `json.Marshaler` with wire-compatible output
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:ConvertBytesSlice:64-126` — the batch CGo optimization that any restructuring must preserve or improve upon
