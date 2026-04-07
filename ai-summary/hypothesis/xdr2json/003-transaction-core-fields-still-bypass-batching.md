# H003: The Mandatory Result/Envelope/Meta Path Still Bypasses xdr2json Batching

**Date**: 2026-04-07
**Subsystem**: xdr2json
**Severity**: Low
**Impact**: latency / CPU / allocations
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Every JSON `getTransactions` response should amortize xdr2json setup for the three mandatory transaction-core fields instead of invoking the single-item bridge separately for result, envelope, and meta on every returned transaction. Even pages with few or no events should not pay three independent `xdr_to_json` calls per transaction for data that is always present.

## Mechanism

`processTransactionsInLedger` always calls `transactionToJSON(tx)`, and `transactionToJSON` still does three sequential `ConvertBytes` calls for `TransactionResult`, `TransactionEnvelope`, and `TransactionMeta`. The existing batch API cannot help because it only handles homogeneous type slices, so the always-hit core path still pays three independent `C.CBytes` input copies, three `C.CString` type-name allocations, three `xdr_to_json` result structs, and three output bridges per transaction. A fixed-shape `xdr_transaction_parts_to_json` entrypoint or a page-level `ConvertTransactionPartsSlice` API would target the one JSON conversion path that every `getTransactions` request exercises, rather than the event-heavy subset only.

## Trigger

1. Run `getTransactions` with `format=json` over ordinary pages that have few events as well as Soroban-heavy pages.
2. Count single-item `xdr_to_json` invocations attributable to `transactionToJSON` and compare them with a prototype that converts `(result,envelope,meta)` in one FFI call per transaction or one batched call per page.
3. Measure whether reducing the always-present triplet path moves end-to-end latency more reliably than event-only batching.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:149-180` — every JSON transaction flows through `transactionToJSON` before events are handled.
- `cmd/stellar-rpc/internal/methods/json.go:transactionToJSON:12-36` — the core fields still use three independent `ConvertBytes` calls.
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:ConvertBytes/convertAnyBytes:36-42,128-147` — single-item conversion still pays full FFI setup each time.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:xdr_to_json:126-160` — Rust still allocates and returns a standalone `ConversionResult` per core-field conversion.

## Evidence

Unlike event conversion, this triplet path is guaranteed on every JSON transaction, including low-event pages where previous event batching ideas are diluted by workload mix. The code already shows the gap clearly: event families can use `ConvertBytesSlice`, but the transaction's three core XDR blobs still bypass batching entirely.

## Anti-Evidence

This requires a new heterogeneous or fixed-shape FFI contract rather than a small local refactor. It also does not remove the actual XDR parse or JSON serialization work, so the gain depends on how much of the current core-field cost is bridge overhead versus payload-sized conversion work.
