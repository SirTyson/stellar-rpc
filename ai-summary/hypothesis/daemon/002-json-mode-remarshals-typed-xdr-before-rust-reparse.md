# H002: JSON getTransactions Re-encodes Typed XDR Before Rust Parses It Again

**Date**: 2026-04-07
**Subsystem**: daemon
**Severity**: Medium
**Impact**: redundant serialization / JSON-mode CPU
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When `getTransactions` is serving `format=json`, the handler should convert each transaction's already-decoded XDR structures to JSON without first re-serializing them back into XDR byte slices and then reparsing those bytes in Rust. Each transaction component should be parsed once per request, not decode in Go, encode to bytes, then decode again in the FFI layer.

## Mechanism

After `LedgerCloseMeta` has already been unmarshaled and `newLedgerTransactionReader` has produced typed `ingestTx` values, the JSON path calls `db.ParseTransaction`, which `MarshalBinary()`-encodes `TransactionResult`, `TransactionMeta`, `TransactionEnvelope`, and every event into fresh byte slices. `transactionToJSON`, `jsonifySlice`, and `BuildEventsJSONFromTransaction` then hand those bytes to `xdr2json`, whose Rust side calls `xdr::Type::read_xdr_to_end(...)` before `serde_json::to_string(...)`. That means JSON mode pays a full Go encode plus a second Rust decode for the same logical data on every transaction and event.

## Trigger

Benchmark `getTransactions` with `format=json`, `limit=200`, and event-heavy ledgers, then compare the current path to a Rust-side page extractor that consumes raw `LedgerCloseMeta` bytes (or another typed one-pass representation) and emits all per-transaction JSON fields without going through `db.ParseTransaction`'s `MarshalBinary` round-trip.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:149-187` — JSON mode routes every transaction through `db.ParseTransaction`, `transactionToJSON`, and event JSON helpers.
- `cmd/stellar-rpc/internal/db/transaction.go:ParseTransaction:238-317` — re-encodes result, meta, envelope, diagnostic events, transaction events, and contract events into `[]byte`.
- `cmd/stellar-rpc/internal/methods/json.go:transactionToJSON/jsonifySlice/jsonifySliceOfSlices:12-37,56-90` — feeds those byte slices into `xdr2json`.
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:ConvertBytes/ConvertBytesSlice:37-44,65-132` — Go FFI entrypoints for byte-based conversion.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:xdr_to_json/xdr_batch_to_json:128-153,208-239` — reparses each XDR buffer with `read_xdr_to_end` before JSON serialization.

## Evidence

The code path is explicit: `processTransactionsInLedger` first builds typed `ingestTx`, then `db.ParseTransaction` marshals those typed values back into byte slices, and `xdr2json` immediately parses the byte slices back into Rust XDR types. The existing batch FFI helper only amortizes call overhead; it does not remove this encode/decode round-trip.

## Anti-Evidence

This only affects `format=json`; the default base64/XDR mode does not use `db.ParseTransaction` or `xdr2json`. The current FFI path already avoids input-side `C.CBytes` copies, so the remaining win comes from eliminating repeated XDR encode/decode work rather than from large C-heap copy savings.
