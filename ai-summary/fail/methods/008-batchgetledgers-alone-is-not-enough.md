# H008: Replacing `BatchGetLedgerMetas` With `BatchGetLedgers` Alone Does Not Remove Enough Work

**Date**: 2026-04-07
**Subsystem**: methods
**Severity**: Low
**Impact**: CPU / allocations
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

If `getTransactions` switches away from `BatchGetLedgerMetas`, the replacement should let the handler avoid decoding or fetching work it no longer needs. A useful optimization must do more than move the same full-ledger decode from the DB helper into the caller.

## Mechanism

At first glance, `BatchGetLedgers` looks attractive because it avoids full `xdr.LedgerCloseMeta` decoding in `db/ledger.go` and already has a benchmark. But `processTransactionsInLedger` requires a typed `xdr.LedgerCloseMeta`, while `BatchGetLedgers` returns only raw `Lcm` bytes plus a parsed header. A naive swap would still force `getTransactions` to fully unmarshal each ledger before calling `ingest.NewLedgerTransactionReaderFromLedgerCloseMeta`, so the same number of ledgers would still be decoded; the work would merely move out of `BatchGetLedgerMetas`.

## Trigger

1. Replace `readTx.BatchGetLedgerMetas(...)` with `readTx.BatchGetLedgers(...)`.
2. Add the missing per-ledger `xdr.LedgerCloseMeta` unmarshal in the handler before `processTransactionsInLedger`.
3. Compare the result against the current implementation on dense and sparse scans.

## Target Code

- `cmd/stellar-rpc/internal/db/ledger.go:73-116` — `BatchGetLedgers` returns `LedgerMetadataChunk` with raw `Lcm` bytes and a parsed header.
- `cmd/stellar-rpc/internal/db/ledger.go:118-140` — `BatchGetLedgerMetas` already does the full decode eagerly.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:75-130` — `processTransactionsInLedger` requires `xdr.LedgerCloseMeta`.
- `cmd/stellar-rpc/internal/methods/get_ledgers.go:204-214` — prior art showing `BatchGetLedgers` is suited to callers that can work directly from `LedgerMetadataChunk`.

## Evidence

The signatures line up badly for a direct substitution: `getLedgers` can consume `LedgerMetadataChunk` because it only needs header and raw bytes, while `getTransactions` needs the fully-typed close meta to build an ingest reader and count transactions. That means the apparent optimization does not by itself reduce how many ledgers are selected or how many full `LedgerCloseMeta` objects must ultimately be decoded.

## Anti-Evidence

`BatchGetLedgers` is still a useful building block for a broader redesign that also changes ledger selection or adds lazy decode. Its header-only parsing benchmark proves the raw-chunk path is real; it just is not, by itself, a meaningful `getTransactions` optimization.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The direct swap does not eliminate the expensive part of the `getTransactions` path: every ledger that reaches `processTransactionsInLedger` still needs a full `xdr.LedgerCloseMeta` object. Without changing ledger selection or adding a true lazy/streaming decode path, this idea mostly just relocates the same decode cost.

### Lesson Learned

When a neighboring endpoint exposes a cheaper data shape, check whether the consuming endpoint can actually use that shape end-to-end. Reusing a helper from `getLedgers` is only valuable here if it also avoids full close-meta decoding or helps skip whole ledgers, not if it merely moves the decode to a different layer.
