# H002: JSON pages still convert result, envelope, and meta one FFI call per transaction

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Medium
**Impact**: CPU / CGo / allocation overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

For `getTransactions` JSON responses, the conversion of transaction result, envelope, and meta should be batched across the whole page. Returning `N` transactions should use roughly three FFI conversions total for these core fields, not three independent CGo crossings per transaction.

## Mechanism

Inside the per-transaction loop, `processTransactionsInLedger()` calls `transactionToJSON()`, and `transactionToJSON()` performs three separate `xdr2json.ConvertBytes()` calls for `Result`, `Envelope`, and `Meta`. The xdr2json layer already exposes `ConvertBytesSlice()` specifically to amortize CGo and Rust type-resolution overhead across a homogeneous batch, so `getTransactions` is leaving a page-level optimization on the table: it could collect all result blobs, all envelope blobs, and all meta blobs for the page, convert each slice once, then assign the returned JSON back by index.

## Trigger

Call `getTransactions` with `xdrFormat=json` and a large page limit over dense ledgers. A CPU profile should show hundreds of short `xdr_to_json`/CGo calls for the core transaction fields, even though the response already has all items buffered before it is returned.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:147-170` — per-transaction JSON conversion occurs in the hot append loop
- `cmd/stellar-rpc/internal/methods/json.go:transactionToJSON:12-36` — three individual `ConvertBytes()` calls per transaction
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:ConvertBytesSlice:61-126` — existing batch API that can collapse many conversions into one CGo call per field type
- `cmd/stellar-rpc/internal/xdr2json/conversion_test.go:95-125` — benchmark already compares individual conversions against the batch path

## Evidence

`processTransactionsInLedger()` converts JSON inline while it is still appending each `TransactionInfo` (`cmd/stellar-rpc/internal/methods/get_transactions.go:147-170`). `transactionToJSON()` then invokes `ConvertBytes()` three times serially for every transaction (`cmd/stellar-rpc/internal/methods/json.go:12-36`). In contrast, `ConvertBytesSlice()` is explicitly implemented to batch homogeneous byte buffers through one CGo call and one Rust type-resolution step (`cmd/stellar-rpc/internal/xdr2json/conversion.go:61-126`), and the conversion benchmark already treats that batched path as a meaningful optimization target (`cmd/stellar-rpc/internal/xdr2json/conversion_test.go:95-125`).

## Anti-Evidence

This only affects JSON-format requests; the default XDR/base64 path does not pay this FFI cost. Any fix has to preserve transaction order and still surface conversion failures with enough context to identify which page item failed.
