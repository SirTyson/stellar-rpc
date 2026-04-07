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
