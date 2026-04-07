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

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: FAIL — substantially covered by fail/012 (UnsafeMarshalBinary incompatible with batching), fail/010 (event batching scope), and fail/006 (input copy removal showed no measurable improvement)
**Failed At**: reviewer

### Trace Summary

Traced the `getTransactions` JSON path from `processTransactionsInLedger` (get_transactions.go:152-162) through `db.ParseTransaction` → `parseEvents` (transaction.go:305-341) which marshals every event via `MarshalBinary()` into `[]byte` slices held in `db.Transaction`. These are then collected page-wide and batch-converted by `batchConvertTransactionsToJSON` (json.go:105-211), which makes exactly 6 `ConvertBytesSlice` CGo calls for the entire page. The hypothesis's proposed mechanism is fundamentally incompatible with this page-level batching architecture, and its target code references are partially outdated.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:152-162` — JSON branch calls `db.ParseTransaction` then defers conversion via `pendingTxJSON`; does NOT use `BuildEventsJSONFromTransaction` (hypothesis target is outdated)
- `cmd/stellar-rpc/internal/methods/get_transactions.go:362-371` — `batchConvertTransactionsToJSON` called once after all transactions collected, NOT per-transaction
- `cmd/stellar-rpc/internal/db/transaction.go:305-341` — `parseEvents` calls `MarshalBinary()` per event, allocating individual `[]byte` slices; these must all be alive simultaneously for page-level batching
- `cmd/stellar-rpc/internal/methods/json.go:105-211` — `batchConvertTransactionsToJSON` flattens all events across all transactions into flat `[][]byte` arrays, then 3 `ConvertBytesSlice` calls for events + 3 for core fields
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:65-133` — `ConvertBytesSlice` pins every `[]byte` backing array via `runtime.Pinner` and passes all in one CGo call; all slices must be valid simultaneously

### Why It Failed

Three independent reasons:

1. **Outdated target code**: The hypothesis references `BuildEventsJSONFromTransaction()` and per-transaction `jsonifySlice()` calls as the `getTransactions` JSON path. The current code uses `batchConvertTransactionsToJSON` which already performs page-level event batching (exactly 3 batch calls for all events across the entire page, not per-transaction). `BuildEventsJSONFromTransaction` is only used by the singular `getTransaction` endpoint, which is out of scope.

2. **Proposed mechanism incompatible with architecture**: The hypothesis proposes `UnsafeMarshalBinary()` or a "scratch arena" to avoid per-event `[]byte` allocations. Fail/012 already proved this is incompatible with page-level batching: `UnsafeMarshalBinary` overwrites its internal buffer on each call (documented: "Subsequent calls to marshaling methods will overwrite the returned buffer"), so all N events cannot be alive simultaneously as required by `ConvertBytesSlice`'s pinning model. A custom arena would require Go XDR types to support marshal-into-preallocated-buffer, which the SDK does not provide.

3. **Savings bounded by prior rejections**: Even the much larger optimization in fail/006 (eliminating two full input copies per conversion — ~50-500ns allocator cost + memcpy proportional to payload size) showed no measurable end-to-end improvement in `getTransactions` benchmarks (throughput ceiling unchanged at 100 RPS, p50 flat-to-worse). This hypothesis targets a strictly SMALLER savings (only the per-event `MarshalBinary()` output allocation of ~50-200ns, not the serialization work itself), making measurable improvement implausible.

### Lesson Learned

When proposing to eliminate intermediate allocations in a batched pipeline, verify that (a) the current batch architecture actually requires those allocations to be alive simultaneously (it does — page-level batching via `ConvertBytesSlice` pins all slices), and (b) the target code paths match the current architecture (the `getTransactions` JSON path was refactored from per-transaction to page-level batching, invalidating hypothesis references to `BuildEventsJSONFromTransaction`). The per-event `MarshalBinary()` allocation is the minimum cost to produce independent byte slices compatible with the batch API — it cannot be eliminated without either changing the batch API to accept typed Go values or implementing a custom arena-based serializer, both of which are disproportionate to the ~0.05-0.2ms allocation savings on a 50-200ms request.
