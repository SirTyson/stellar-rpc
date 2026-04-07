# H003: Event-heavy JSON pages still cross the FFI boundary once per transaction

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Medium
**Impact**: CPU / CGo / allocation overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

For JSON-format `getTransactions`, diagnostic events, transaction events, and contract events should be converted in page-wide batches. An event-heavy page should pay a small constant number of FFI conversions per event type, not one conversion round per returned transaction.

## Mechanism

`processTransactionsInLedger()` calls `jsonifySlice()` for diagnostic events and then `BuildEventsJSONFromTransaction()` for every transaction. `BuildEventsJSONFromTransaction()` batches contract events only within a single transaction, then repeats the same conversion pattern for the next transaction, so a page with `N` event-bearing transactions still performs O(N) batch conversions and repeated CGo setup. Because `ConvertBytesSlice()` already preserves ordering for homogeneous slices, the handler can flatten page-wide diagnostic/transaction/contract event byte buffers, convert them once per event type, and then split the results back by transaction and operation offsets.

## Trigger

Call `getTransactions` with `xdrFormat=json` over ledgers containing many Soroban transactions with diagnostic, transaction, and contract events. A profile should show repeated `xdr_batch_to_json` work originating from each transaction’s event conversion helpers rather than a few large batched conversions for the page.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:157-170` — per-transaction diagnostic/event JSON conversion in the hot loop
- `cmd/stellar-rpc/internal/methods/get_transaction.go:BuildEventsJSONFromTransaction:143-155` — event conversion helper called separately for each transaction
- `cmd/stellar-rpc/internal/methods/json.go:jsonifySlice/jsonifySliceOfSlices:56-90` — batching is currently limited to a single transaction’s inner slices
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:ConvertBytesSlice:61-126` — batch converter already supports the needed slice semantics

## Evidence

The JSON branch in `processTransactionsInLedger()` converts diagnostic events via `jsonifySlice()` and then calls `BuildEventsJSONFromTransaction()` for every transaction (`cmd/stellar-rpc/internal/methods/get_transactions.go:157-170`). `BuildEventsJSONFromTransaction()` in turn invokes `jsonifySliceOfSlices()` for contract events and `jsonifySlice()` for transaction events, but only for that one transaction (`cmd/stellar-rpc/internal/methods/get_transaction.go:143-155`). `jsonifySliceOfSlices()` does flatten inner operation slices, yet it rebuilds that flattened buffer anew for each transaction instead of across the whole page (`cmd/stellar-rpc/internal/methods/json.go:60-90`). The underlying converter is already optimized for large homogeneous batches (`cmd/stellar-rpc/internal/xdr2json/conversion.go:61-126`), so the remaining inefficiency is the page-level call pattern, not missing lower-level support.

## Anti-Evidence

Classic transactions with no events will see little or no benefit from this change. Any page-wide batching fix must preserve the exact nested per-transaction / per-operation grouping and the empty-array behavior expected by the RPC protocol.
