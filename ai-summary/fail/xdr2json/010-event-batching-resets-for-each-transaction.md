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

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — distinct target (events) from fail/005 (core fields), but same fundamental mechanism
**Failed At**: reviewer

### Trace Summary

Traced the per-transaction event conversion path from `processTransactionsInLedger` (get_transactions.go:149-186) through three per-transaction batch sites: `jsonifySlice(DiagnosticEvent, tx.Events)` at line 167, `jsonifySliceOfSlices(ContractEvent, tx.ContractEvents)` inside `BuildEventsJSONFromTransaction` (get_transaction.go:147), and `jsonifySlice(TransactionEvent, tx.TransactionEvents)` (get_transaction.go:151). Each delegates to `ConvertBytesSlice` (conversion.go:65-132) which performs a full CGo round-trip including Go-side scaffolding allocation (`items`, `indices`, `pinner`), `C.CString` type name, and one `xdr_batch_to_json` call (lib.rs:208-289) with its own `catch_unwind`, `TypeVariant::from_str`, `Vec::with_capacity`, and `Box::new(BatchConversionResult)`. Quantified all fixed per-call costs against the data-proportional serialization work that dominates each batch call.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:114-186` — per-transaction loop issues 3 batch calls for event families (diagnostic, contract, transaction) per transaction
- `cmd/stellar-rpc/internal/methods/get_transaction.go:142-156` — `BuildEventsJSONFromTransaction` issues 2 batch calls: `jsonifySliceOfSlices` for contract events (which flattens inner slices but only within one transaction), `jsonifySlice` for transaction events
- `cmd/stellar-rpc/internal/methods/json.go:56-91` — `jsonifySlice` delegates directly to `ConvertBytesSlice`; `jsonifySliceOfSlices` flattens `[][][]byte` → `[][]byte` before one `ConvertBytesSlice` call
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:65-132` — `ConvertBytesSlice` allocates per-call: `result` slice, `items` (C.xdr_t) slice, `indices` slice, `runtime.Pinner`; builds CString type name; makes single CGo call; extracts per-item results
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:208-289` — `xdr_batch_to_json` per-call: `catch_unwind` setup (~50-100ns), `from_c_string` + `TypeVariant::from_str` (~155-265ns per fail/003), `Vec::with_capacity(count)`, per-item `read_xdr_to_end` + `serde_json::to_string` (data-proportional: ~1-200μs per item), `Box::new(BatchConversionResult)` (~20-50ns)

### Why It Failed

This hypothesis applies the same batching-scope argument from fail/005 (core fields) to the event families. The cost analysis yields an identical conclusion:

**Fixed per-batch-call overhead (amortizable):**
- CGo crossing: ~200-400ns
- `C.CString` type name alloc/free: ~50-80ns
- `TypeVariant::from_str`: ~155-265ns
- `catch_unwind`: ~50-100ns
- `Box::new(BatchConversionResult)`: ~20-50ns
- Go-side `items`/`indices`/`result` slice allocation: ~100-200ns
- **Total: ~575-1095ns ≈ 0.6-1.1μs per batch call**

**For a 200-transaction page with 3 event batch calls per transaction:**
- Current: 600 batch calls → 600 × 0.6-1.1μs = 360-660μs fixed overhead
- Proposed: 3 batch calls → 3 × 0.6-1.1μs = 1.8-3.3μs fixed overhead
- **Savings: ~358-657μs ≈ 0.4-0.7ms**

**Data-proportional work (irreducible, identical in both designs):**
- Per event item: XDR deserialization + JSON serialization = ~1-200μs depending on event size
- For 1000 events (200 txs × 5 events): ~1-200ms
- Plus output bridge per item: ~20-50ns × 1000 = ~20-50μs (already optimized by ptr+len approach)

**Savings as fraction of total request (50-200ms): 0.2-1.4%**

This is the same regime as fail/005's conclusion: "below the noise floor of realistic benchmarks (typically 1-5% variance)." The fixed per-call overhead being eliminated is ~0.6-1.1μs per call, while each call's data-proportional work is ~5-1000μs — a 8-1600× ratio. Batching can only eliminate the smaller component.

Additionally, page-wide event accumulation adds non-trivial Go-side complexity: the code must track per-transaction event counts for each of 3 event families, accumulate `[][]byte` and `[][][]byte` across transactions, make 3 page-wide `ConvertBytesSlice` calls, then re-split the returned `[]json.RawMessage` slices back into per-transaction `TransactionInfo` structs. This bookkeeping overhead partially offsets the fixed-cost savings it's trying to eliminate.

### Lesson Learned

The pattern "per-transaction batching resets batch scaffolding O(N) times" recurs for both core fields (fail/005) and event families (this investigation). In both cases, the savings from reducing O(N) to O(1) batch calls are ~0.3-0.7ms for a 200-tx page — below the noise floor for a 50-200ms request. The fundamental reason is that xdr2json's cost is dominated by data-proportional XDR parsing and JSON serialization work (~1-200μs per item), making the ~0.6-1.1μs per-call fixed overhead negligible regardless of how many times it repeats. Future optimization effort should focus on reducing the per-item data-proportional cost (serialization strategy, output format, or avoiding conversion entirely) rather than further consolidating batch call boundaries.
