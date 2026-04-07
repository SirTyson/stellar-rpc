# H005: strconv Round-Trips in getTransactions Pagination Are a Meaningful Hot-Path Cost

**Date**: 2026-04-07
**Subsystem**: daemon
**Severity**: Low
**Impact**: per-request allocation waste
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Numeric pagination and ledger-sequence normalization in `getTransactions` should use direct integer casts or bounds checks, not string-based `strconv` round-trips. The request path should avoid allocating temporary decimal strings for fixed-width conversions.

## Mechanism

`uint32ToInt32` converts through `strconv.FormatUint` plus `strconv.ParseInt`, and `processTransactionsInLedger` similarly converts `limit` through `FormatUint` plus `Atoi`. At first glance this looks like avoidable per-request allocation churn in a hot path.

## Trigger

Benchmark `getTransactions` at high QPS with small pages and compare current code to a version that replaces the string round-trips with direct range checks and casts.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:uint32ToInt32:33-38` — converts `uint32` through a decimal string.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:84-87` — converts `limit` through a second decimal string before comparing lengths.

## Evidence

Both conversions are visibly string-based, so they do allocate temporary decimal text instead of doing direct integer arithmetic. They also occur on the request path rather than at startup.

## Anti-Evidence

These conversions happen only a handful of times per request, while the same request also performs SQLite reads, `LedgerCloseMeta` decoding, per-ledger envelope hashing, and XDR/JSON serialization. Their absolute cost is therefore in the nanosecond-to-sub-microsecond range and far too small to explain a measurable `getTransactions` latency or RPS change.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The string round-trips are real dead work, but they execute too infrequently and touch too little data to matter next to ledger fetch, transaction reader setup, and serialization costs. Even a perfect fix would be well below the pipeline's measurable-improvement threshold.

### Lesson Learned

For `getTransactions`, prioritize work that scales with page size or ledger size: full-meta fetches, envelope hashing, and FFI crossings. Scalar cleanup that runs only once or a few times per request is not a strong optimization target here.
