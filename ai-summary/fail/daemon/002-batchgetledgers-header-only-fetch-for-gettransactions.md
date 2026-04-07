# H002: Reusing BatchGetLedgers Would Eliminate getTransactions Ledger Decode Cost

**Date**: 2026-04-07
**Subsystem**: daemon
**Severity**: Medium
**Impact**: deserialization cost
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

If `getTransactions` only needed ledger headers plus raw metadata bytes, it could follow the `getLedgers` pattern and avoid fully decoding each `LedgerCloseMeta` before iterating the range. That would make a header-only batched fetch a plausible replacement for `BatchGetLedgerMetas`.

## Mechanism

I investigated whether the newer `BatchGetLedgers` API could replace `BatchGetLedgerMetas` on the `getTransactions` path so the daemon would fetch lighter `LedgerMetadataChunk` values and defer expensive decode work. That would only be a real optimization if transaction iteration could proceed directly from the raw bytes or header-only structure.

## Trigger

Attempt to switch `getTransactionsByLedgerSequence` to `BatchGetLedgers` and thread `LedgerMetadataChunk` through `processTransactionsInLedger`.

## Target Code

- `cmd/stellar-rpc/internal/db/ledger.go:BatchGetLedgers:73-116` — returns raw `meta` bytes plus a decoded header.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:74-231` — consumes a fully decoded `xdr.LedgerCloseMeta`.
- `cmd/stellar-rpc/internal/methods/get_ledgers.go:fetchLedgers:204-214` — shows where `BatchGetLedgers` is viable today.

## Evidence

`BatchGetLedgers` already exists and is used successfully by `getLedgers`, so at first glance it looks like a reusable lower-allocation batch-fetch primitive.

## Anti-Evidence

`processTransactionsInLedger` immediately calls `ingest.NewLedgerTransactionReaderFromLedgerCloseMeta(h.networkPassphrase, ledger)`, which requires a materialized `xdr.LedgerCloseMeta` to iterate transactions and extract events/results.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

`getTransactions` genuinely needs the decoded `LedgerCloseMeta`; the header-only batch API works for `getLedgers` because that method can serve raw ledger bytes, but it does not remove any required decode from the transaction-walking path.

### Lesson Learned

Do not borrow `getLedgers` optimizations blindly for `getTransactions`. Optimizations are only real wins here if they eliminate duplicated wrapper work or repeated conversions, not if they merely move an unavoidable `LedgerCloseMeta` decode to a different layer.
