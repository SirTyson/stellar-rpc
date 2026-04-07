# H002: Event JSON Conversion Still Materializes `[][]byte` / `[][][]byte` Before the Batch FFI Call

**Date**: 2026-04-07
**Subsystem**: xdr2json
**Severity**: Medium
**Impact**: latency / CPU / allocations / GC
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once `getTransactions` has typed `DiagnosticEvent`, `TransactionEvent`, and
`ContractEvent` values in memory, the JSON path should batch-convert those typed
events without first allocating standalone XDR byte slices for every event. A
Soroban-heavy page should not create thousands of short-lived `[]byte` objects
whose only consumer is the immediately following `ConvertBytesSlice()` call.

## Mechanism

`ingestTx.GetTransactionEvents()` already returns typed event collections, but
`parseEvents()` eagerly marshals every event into `tx.Events`,
`tx.TransactionEvents`, and `tx.ContractEvents`. `BuildEventsJSONFromTransaction()`
and `jsonifySlice()` then turn around and batch-convert those byte slices through
xdr2json. A typed batch helper that serializes events into a request-scoped
scratch arena or uses `EncodingBuffer.UnsafeMarshalBinary()` to build the batch
input right before the FFI call would remove one heap object and one Go-side copy
per event while preserving the existing batched Rust conversion.

## Trigger

1. Issue `getTransactions` with `xdrFormat=json` against ledgers with many small
   diagnostic, contract, and transaction events.
2. Profile allocations inside `parseEvents()`, especially the per-event
   `MarshalBinary()` calls and the growth of `tx.Events`, `tx.TransactionEvents`,
   and `tx.ContractEvents`.
3. Compare against a prototype that bypasses those intermediate slices and feeds
   typed event batches directly into a methods-local xdr2json helper.

## Target Code

- `cmd/stellar-rpc/internal/db/transaction.go:parseEvents:304-340` — every event is copied into its own `[]byte` before any JSON conversion begins.
- `cmd/stellar-rpc/internal/methods/get_transaction.go:BuildEventsJSONFromTransaction:143-155` — event JSON conversion consumes the intermediate byte slices immediately.
- `cmd/stellar-rpc/internal/methods/json.go:jsonifySlice/jsonifySliceOfSlices:56-90` — current helpers require fully materialized `[][]byte` input.
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/xdr/main.go:EncodingBuffer.UnsafeMarshalBinary:188-194` — reusable scratch serialization already exists and can support a typed batch bridge.

## Evidence

The code path is already split into "extract typed events" and then "marshal them
back into bytes for xdr2json", which means there is a dedicated intermediate
representation with no independent value to the JSON response. On event-heavy
pages, that representation scales linearly with event count and payload size
before Rust does any work.

## Anti-Evidence

This optimization only helps the JSON event fields; it does not change the
mandatory `ResultJSON` / `EnvelopeJSON` / `ResultMetaJSON` conversions. It also
requires a methods-layer helper or a new xdr2json typed-batch API, because the
current shared `db.Transaction` shape is used by non-JSON callers too.
