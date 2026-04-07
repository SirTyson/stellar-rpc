# H003: JSON getTransactions Re-enters xdr2json Per Transaction Instead of Per Page

**Date**: 2026-04-07
**Subsystem**: daemon
**Severity**: High
**Impact**: FFI boundary / serialization overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

For `getTransactions` in `format=json`, the daemon should batch homogeneous XDR-to-JSON conversion across the whole response page so that a 50-200 transaction response crosses the CGo/Rust boundary a small constant number of times. The hot path should not make separate FFI calls for each transaction's result, envelope, and meta when the batch API already exists.

## Mechanism

Inside `processTransactionsInLedger`, the JSON path calls `transactionToJSON(tx)` for every transaction, and that helper performs three separate `xdr2json.ConvertBytes(...)` calls (`TransactionResult`, `TransactionEnvelope`, `TransactionMeta`). Event conversion is only batched within each individual transaction, not across the full page. As a result, a 200-transaction JSON page still performs hundreds of CGo crossings and C-side input copies even though `xdr2json.ConvertBytesSlice` already exists specifically to amortize boundary overhead across many same-typed payloads.

## Trigger

Benchmark `getTransactions` with `format=json` at limits near `50` and `200`, especially on transactions that also carry diagnostic or contract events. Compare the current code to a version that accumulates `Result`, `Envelope`, `Meta`, and event byte slices for the whole page and calls `ConvertBytesSlice` once per XDR type before scattering the JSON results back into `protocol.TransactionInfo`.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:150-187` — per-transaction JSON path invokes `transactionToJSON`, `jsonifySlice`, and `BuildEventsJSONFromTransaction`.
- `cmd/stellar-rpc/internal/methods/json.go:12-36` — `transactionToJSON` does three one-item `ConvertBytes` calls per transaction.
- `cmd/stellar-rpc/internal/methods/json.go:56-90` — batching helpers already exist, but only for slices passed within a single transaction.
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:61-125` — `ConvertBytesSlice` provides the batch CGo path that can replace many single-item `ConvertBytes` calls.

## Evidence

The current JSON path performs one CGo call per `Result`, one per `Envelope`, and one per `Meta` for every transaction in the page, even though the conversion layer already has a `ConvertBytesSlice` API designed to batch same-typed payloads. That means the code is paying repeated `C.CBytes`, `xdr_to_json`, and Go/C boundary costs that scale linearly with transaction count instead of with field types.

## Anti-Evidence

This only helps `format=json`; XDR responses already stay on the Go side and use the reusable `xdr.EncodingBuffer`. The batching rewrite also needs careful bookkeeping to preserve per-transaction error attribution and to split flattened event results back into the existing response shape.
