# H016: Rust envelope flattening before JSON extraction is too small to matter

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Low
**Impact**: allocation / iterator overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

The raw-LCM JSON extractor should avoid temporary envelope collections if they materially affect `getTransactions` latency. If envelope traversal is a hot cost, the Rust path should walk the generalized transaction set directly instead of first collecting references into a temporary vector.

## Mechanism

I suspected `extract_envelopes()` was creating avoidable work because it flattens the generalized transaction set into `Vec<&TransactionEnvelope>` and `extract_transactions_json()` then immediately loops that vector again. If that extra allocation and pass were large enough, fusing the traversal with JSON extraction could reduce per-ledger overhead.

## Trigger

Call `getTransactions(format=json)` over multi-ledger pages and inspect the Rust extraction path inside `lcm_transactions_to_json`, focusing on `extract_envelopes()` and the subsequent per-transaction JSON loop.

## Target Code

- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:extract_envelopes:409-429` — flattens generalized tx-set phases/stages into a temporary vector of envelope references.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:extract_transactions_json:477-556` — immediately loops the collected envelope vector and serializes result/meta/envelope/events.

## Evidence

`extract_envelopes()` does build a `Vec<&TransactionEnvelope>` for V1/V2 transaction sets before any JSON is emitted (`cmd/stellar-rpc/lib/xdr2json/src/lib.rs:409-429`). `extract_transactions_json()` then iterates that vector and performs the real work (`cmd/stellar-rpc/lib/xdr2json/src/lib.rs:505-556`), so there is a genuine temporary allocation plus an extra structural pass on paper.

## Anti-Evidence

The temporary vector holds only references, not deep copies, and the subsequent loop performs much heavier work: `serde_json::to_value` for result/meta/envelope/events plus final `serde_json::to_string` for the ledger result. Recent retained-workload notes in `ai-summary/fail/db/015-ledger-json-fast-path-ignores-page-bounds.md` also show the practical data shape is sparse (`avg_tx_per_ledger ≈ 1`, `max_tx_per_ledger = 2`), which keeps the temporary vector tiny on the benchmark workload.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The extra work exists, but it is only a small vector of references plus one additional structural traversal ahead of much larger per-transaction JSON serialization. On the real sparse retained workload, the vector is typically length 1-2, so removing it would not produce a meaningful `getTransactions` improvement.

### Lesson Learned

For the raw-LCM JSON path, small iterator/collection cleanup in Rust is only worth tracking if it eliminates the dominant `serde_json` conversion work or repeated per-ledger DB/FFI activity. Pure envelope-reference flattening does neither.
