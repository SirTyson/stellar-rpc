# H004: JSON getTransactions Fans Out Into Many Small CGo XDR-to-JSON Calls Per Transaction

**Date**: 2026-04-06
**Subsystem**: methods
**Severity**: High
**Impact**: latency / CPU / FFI overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

The JSON form of `getTransactions` should minimize CGo boundary crossings by converting a transaction payload in coarse batches, ideally once per transaction or once per ledger. A 50-200 transaction page should not require separate Rust FFI calls for every result blob, envelope, meta blob, diagnostic event, transaction event, and contract event.

## Mechanism

For each transaction, `processTransactionsInLedger` calls `transactionToJSON` (three `ConvertBytes` calls), `jsonifySlice` for diagnostic events, and `BuildEventsJSONFromTransaction`, which makes additional `ConvertBytes` calls for every transaction and contract event. `xdr2json.ConvertBytes` allocates C memory with `C.CBytes`, allocates a type-name string with `C.CString`, crosses into `xdr_to_json`, and copies the JSON result back on every call. On event-heavy JSON pages this turns one RPC into hundreds or thousands of tiny CGo conversions, which should be materially slower than a batched serializer in the shared `xdr2json` crate.

## Trigger

1. Issue `getTransactions` with `format=json` against ledgers containing many Soroban transactions with contract and diagnostic events.
2. Profile CGo call counts, CPU time, and allocation volume for a 50-transaction and 200-transaction page.
3. Compare against a prototype that batches the per-transaction JSON conversion across the FFI boundary.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:162-191` — JSON path performs multiple conversions per returned transaction.
- `cmd/stellar-rpc/internal/methods/json.go:12-36` — `transactionToJSON` does three separate `ConvertBytes` calls.
- `cmd/stellar-rpc/internal/methods/json.go:56-80` — `jsonifySlice` and `jsonifySliceOfSlices` convert each event individually.
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:36-79` — each conversion allocates C buffers and crosses the FFI boundary.

## Evidence

The loaded optimization guidance explicitly calls out serialization overhead and FFI data-copy costs for `getTransactions`, and this code matches that pattern exactly. The JSON path is a fan-out tree of tiny conversions rather than a coarse-grained serialization step.

## Anti-Evidence

This does not affect XDR responses, and the gain will be smaller for transactions with no events. A batched FFI path would require coordinated changes in the shared `xdr2json` implementation, so the fix is more invasive than a local Go-only optimization.
