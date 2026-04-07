# H003: Event Batching Resets for Every Transaction Instead of Amortizing Across the Page

**Date**: 2026-04-07
**Subsystem**: xdr2json
**Severity**: Low
**Impact**: latency / CPU / allocations
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

The homogeneous xdr2json batch API should amortize diagnostic, contract, and
transaction event conversion across the full `getTransactions` page (or at least
across each fetched ledger chunk), not just inside a single transaction. A
200-transaction JSON page should not perform hundreds of small `xdr_batch_to_json`
calls when a handful of page-wide batches could produce the same JSON fragments.

## Mechanism

`processTransactionsInLedger()` invokes `jsonifySlice()` for diagnostic events and
`BuildEventsJSONFromTransaction()` for contract / transaction events inside the
per-transaction loop. Each helper batches only within one transaction, so the page
still performs O(transaction count) `xdr_batch_to_json()` calls and re-allocates
fresh `items`, `indices`, `Vec<ConversionResult>`, and `BatchConversionResult`
scaffolding for every transaction. Collecting events per page or per ledger chunk
before calling `ConvertBytesSlice()` would cut those repeated batch-setup costs
while reusing the existing homogeneous batch ABI.

## Trigger

1. Issue `getTransactions` with `xdrFormat=json` and a large page limit.
2. Use workloads where many transactions each have a few small diagnostic,
   contract, or transaction events.
3. Compare the current per-transaction batching with a prototype that accumulates
   event families across the page and then reshapes the returned JSON back into
   per-transaction slices.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:149-180` — event conversion happens inside the per-transaction loop.
- `cmd/stellar-rpc/internal/methods/get_transaction.go:BuildEventsJSONFromTransaction:143-152` — two more batch calls are issued per transaction for contract and transaction events.
- `cmd/stellar-rpc/internal/methods/json.go:jsonifySlice/jsonifySliceOfSlices:56-90` — batching scope is limited to the caller-provided per-transaction slices.
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:ConvertBytesSlice:65-132` — every batch call allocates new Go-side batch scaffolding.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:xdr_batch_to_json:208-289` — every batch call repeats Rust-side type resolution, panic boundary setup, result-vector allocation, and batch-result allocation.

## Evidence

The code already batches within one transaction, which implies the fixed batch
setup cost is worth avoiding at least once. But the handler's control flow resets
that batching boundary for every transaction, even though the endpoint eventually
returns one page-wide response and the batch API only cares about homogeneity of
XDR type, not transaction ownership.

## Anti-Evidence

For large event payloads, Rust deserialization and JSON serialization likely still
dominate per-item cost, so the win is most plausible on pages with many small
events rather than giant Soroban metas. Any page-wide implementation also has to
re-split results back into per-transaction / per-operation groupings, which adds
bookkeeping on the Go side.
