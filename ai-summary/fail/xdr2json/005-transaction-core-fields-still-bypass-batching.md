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

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated (though builds on cost analysis from fail/003 and fail/004)
**Failed At**: reviewer

### Trace Summary

Traced the full path from `processTransactionsInLedger` (get_transactions.go:149-159) through `transactionToJSON` (json.go:12-36) into three independent `ConvertBytes` → `convertAnyBytes` calls (conversion.go:36-42, 128-147), each crossing the CGo boundary into `xdr_to_json` (lib.rs:127-161). Confirmed that each call independently pays CGo crossing overhead, type name CString allocation, Rust-side type resolution, `catch_unwind` setup, and `ConversionResult` Box allocation. Quantified each component and compared against the irreducible data-proportional work (XDR deserialization + JSON serialization) that dominates each call.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/json.go:12-36` — `transactionToJSON` makes 3 sequential `ConvertBytes` calls for TransactionResult, TransactionEnvelope, TransactionMeta
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:36-42` — `ConvertBytes` extracts type name via `reflect.TypeOf().Name()` (~5ns, zero alloc) and delegates to `convertAnyBytes`
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:128-147` — `convertAnyBytes` allocates CXDR via `C.CBytes` (data-proportional), `C.CString` for type name (~50-80ns), makes CGo call (~200-400ns round trip), copies results via `C.GoString`
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:127-161` — `xdr_to_json` wraps in `catch_unwind` (~50-100ns), resolves type via `from_c_string` + `TypeVariant::from_str` (~150-265ns per fail/003), deserializes XDR via `read_xdr_to_end` (1-100μs, data-proportional), serializes JSON via `serde_json::to_string` (1-100μs, data-proportional)
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:64-126` — `ConvertBytesSlice` shows the existing batch API is homogeneous-only (single `typename` parameter), confirming it cannot batch the heterogeneous triplet

### Why It Failed

The fixed per-call overhead that batching eliminates (CGo crossing, type name CString, type resolution, catch_unwind, Box alloc/free, empty error string) totals ~0.5-1μs per call. The data-proportional work that cannot be eliminated by batching (XDR deserialization, JSON serialization, input/output copies) totals ~12-220μs per core field, or ~35-650μs per transaction across all three fields.

**Per-transaction savings from 3→1 calls**: ~1-2μs (eliminating 2× fixed overhead)
**Per-page savings for 200 transactions**: ~200-400μs ≈ 0.2-0.4ms
**Total request time**: ~50-200ms
**Savings as fraction of request**: 0.1-0.7%

Even the more ambitious page-level approach (collecting all 200 Results + 200 Envelopes + 200 Metas into 3 batch `ConvertBytesSlice` calls instead of 600 individual calls — which would use the existing API without new FFI contracts) saves only ~0.3-0.6ms, or 0.2-1.2% of total request time.

Both variants fall below the noise floor of realistic benchmarks (typically 1-5% variance), making the improvement unmeasurable in practice. The hypothesis correctly identifies that core fields bypass the batch API, but overestimates the cost of bridge overhead relative to the payload-sized serialization work that dominates each call by 50-1000×. Previous investigations (fail/003: type dispatch at ~155-265ns; fail/004: empty error CString too small to matter) already established that individual components of per-call fixed overhead are negligible — aggregating them does not change this conclusion because the data-proportional work scales identically whether calls are batched or not.

### Lesson Learned

When evaluating "bypass batching" hypotheses, the critical question is what fraction of per-call cost is fixed overhead (amortizable) vs data-proportional (irreducible). In the xdr2json path, fixed overhead is ~0.5-1μs while data-proportional work is ~12-220μs per core field — a 25-440× ratio. Batching can only eliminate the smaller component, producing savings that are mathematically real but practically unmeasurable against the dominant serialization cost. The input copies (`C.CBytes`) are also data-proportional and cannot be avoided even in a batched design, since Go memory cannot be held across CGo boundaries. Future optimization effort should target the data-proportional path (serialization format, output bridge copies) rather than fixed per-call scaffolding.
