# H004: The Success Path's Empty Error CString Is Too Small to Matter

**Date**: 2026-04-07
**Subsystem**: xdr2json
**Severity**: Low
**Impact**: latency / allocations
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Successful single-item xdr2json conversions should not need to allocate or copy any error payload at all. When `getTransactions` converts `TransactionResult`, `TransactionEnvelope`, and `TransactionMeta` successfully, the bridge should ideally return a null error pointer and avoid any success-path work for error propagation.

## Mechanism

`xdr_to_json` always returns `error: string_to_c(result.error)` even when `result.error` is an empty string, and `convertAnyBytes` then unconditionally runs `C.GoString(result.error)`. That means every successful core-field conversion pays for an empty CString allocation, a zero-length Go string materialization, and an eventual `free_c_string`, which looks like needless per-call scaffolding when reading the code.

## Trigger

1. Issue `getTransactions` with `format=json` against a full 200-transaction page.
2. Compare the current path with a prototype that returns `NULL` for `result.error` on success and only calls `C.GoString` when the pointer is non-null.
3. Measure whether the reduced success-path allocator churn changes request latency or allocation counts materially.

## Target Code

- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:xdr_to_json:150-160` — successful conversions currently build an empty Rust error string and always export it as a CString.
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:convertAnyBytes:136-143` — Go always reads `result.error` with `C.GoString`.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:free_conversion_result:169-178` — the empty error string is freed on every successful single-item call.

## Evidence

The success path demonstrably allocates and frees an empty error string for every `ConvertBytes` call, and `transactionToJSON` makes three such calls per transaction. This is easy to spot and straightforward to prototype.

## Anti-Evidence

Only the single-item path is affected, and the string being allocated is zero-length. Even on a 200-transaction page that is only 600 occurrences, which makes this a fixed-cost micro-optimization beside far larger costs like JSON serialization, input copying, and output copying.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The empty-error allocation is real, but its cost is too small and too infrequent to plausibly move `getTransactions` latency by a measurable amount. It sits far below the larger payload-sized costs already present in the same `ConvertBytes` path.

### Lesson Learned

For xdr2json, the promising work is where cost scales with the number of bytes or the number of batched items. Fixed zero-length success-path bookkeeping is easy to notice in code but is not a strong performance hypothesis unless it gates a much larger structural change.
