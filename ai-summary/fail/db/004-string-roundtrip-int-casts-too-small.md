# H004: String round-trips for integer casts are too small to matter

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Low
**Impact**: CPU / allocation overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

The `getTransactions` hot path should convert `uint32`/`uint` values to `int32`/`int` without allocating temporary strings. Scalar casts on a performance-sensitive endpoint should stay in integer space unless an actual formatting boundary is involved.

## Mechanism

I suspected the repeated `strconv.FormatUint(...); strconv.ParseInt(...)` and `strconv.Atoi(...)` patterns were creating avoidable allocations in the common path. If those conversions were a large enough fraction of request time, replacing them with bounds-checked integer casts would remove some per-request churn.

## Trigger

Request large `getTransactions` pages so `ParseTransaction()` runs many times, then inspect allocation profiles for `strconv` helpers involved in `applicationOrder`, `ledger sequence`, and `limit` conversion.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:uint32ToInt32:31-36` — string-backed `uint32` to `int32` conversion helper
- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:81-84` — `limit` converted with `FormatUint` + `Atoi`
- `cmd/stellar-rpc/internal/db/transaction.go:ParseTransaction:245-250` — per-transaction `applicationOrder` conversion uses the same pattern

## Evidence

The current code does allocate through `strconv.FormatUint` before parsing back into an integer in all three sites above (`cmd/stellar-rpc/internal/methods/get_transactions.go:31-36,81-84` and `cmd/stellar-rpc/internal/db/transaction.go:245-250`).

## Anti-Evidence

These are scalar conversions surrounded by much heavier work: ledger-meta deserialization, transaction reader setup, transaction marshaling, event marshaling, and optional FFI JSON conversion. Even on large pages, the endpoint only performs a handful of these conversions per ledger plus one per returned transaction, so the absolute cost stays tiny.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The suspected waste is real but too small relative to the surrounding XDR, hashing, event, and FFI work to produce a meaningful `getTransactions` improvement.

### Lesson Learned

For this endpoint, prefer hypotheses that remove whole-ledger or whole-page work. Scalar formatting cleanup is only worth tracking if it is isolated from much more expensive operations, which is not the case here.
