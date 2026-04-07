# H005: String-Based Integer Conversions in getTransactions Might Be a Hot Allocation Source

**Date**: 2026-04-06
**Subsystem**: methods
**Severity**: Low
**Impact**: CPU / allocations
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Integer conversions in the `getTransactions` hot path should use direct casts with bounds checks instead of allocating decimal strings. Large pages should not spend measurable time formatting integers to strings just to parse them back into another integer width.

## Mechanism

The handler uses `strconv.FormatUint(...)+ParseInt(...)` in `uint32ToInt32`, repeats the same pattern for `applicationOrder` in `ParseTransaction`, and uses `FormatUint(...)+Atoi(...)` for `limitInt` in `processTransactionsInLedger`. If those conversions were on the critical path, every returned transaction and every touched ledger would pay avoidable string allocations.

## Trigger

1. Request large `getTransactions` pages, up to the maximum limit.
2. Compare CPU and allocation profiles against a version that uses direct casts with overflow guards.
3. Focus on workloads that return many transactions per request.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:uint32ToInt32:31-36` — `uint32` to `int32` conversion uses decimal string round-tripping.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:99-102` — `limit` is converted to `int` through string formatting/parsing.
- `cmd/stellar-rpc/internal/db/transaction.go:ParseTransaction:246-250` — `ingestTx.Index` is converted to `int32` the same way.

## Evidence

The conversion helpers are visibly allocation-heavy compared to a simple bounds check and cast, and `ParseTransaction` runs once per returned transaction. That made this a plausible micro-optimization candidate on first inspection.

## Anti-Evidence

These conversions happen only a handful of times per ledger or transaction, while the surrounding work includes ledger fetches, XDR marshaling, base64 encoding, event extraction, and JSON/FFI conversion. Even at the maximum 200-transaction page size, the saved work would be tiny relative to the rest of the endpoint.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-06
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The conversions are mildly inefficient but too small to plausibly produce a measurable `getTransactions` improvement. They are drowned out by DB I/O and serialization costs in the same code path.

### Lesson Learned

For this endpoint, only optimizations that remove ledger fetches, event extraction, reader setup, or serialization work are likely to clear the performance threshold. Scalar conversion cleanups are not enough on their own.
