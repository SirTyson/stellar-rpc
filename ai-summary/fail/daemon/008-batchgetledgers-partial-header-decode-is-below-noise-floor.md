# H008: BatchGetLedgers Partial Header Decode Is a Meaningful Hot-Path Cost

**Date**: 2026-04-07
**Subsystem**: daemon
**Severity**: Low
**Impact**: partial XDR decode overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

`BatchGetLedgers` should avoid partially decoding every ledger blob just to recover metadata that the SQL row already knows, such as the ledger sequence. In the `getTransactions` fast path, range fetching should spend nearly all of its time on raw BLOB transfer and the full decode of ledgers that are actually processed, not on a second lightweight parse of every fetched blob.

## Mechanism

I suspected `BatchGetLedgers` was still paying too much per-ledger CPU because it selects only `meta`, then runs `xdr.Unmarshal` over the version, optional extension, and header for every row before `getTransactions` later does a full `LedgerCloseMeta.UnmarshalBinary` on the ledgers it uses. A variant that selects `sequence, meta` and defers header inspection entirely looked like it might remove a size-proportional prefix parse from each fetched ledger.

## Trigger

Benchmark dense `getTransactions` ranges after changing `BatchGetLedgers` to return the SQL `sequence` column directly and skipping the current per-row header decode.

## Target Code

- `cmd/stellar-rpc/internal/db/ledger.go:(ledgerReaderTx).BatchGetLedgers:88-132` — partially decodes version/extension/header from every `meta` blob.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:336-369` — later fully decodes `chunk.Lcm` for each processed ledger.

## Evidence

The helper does an explicit prefix parse (`xdr.Unmarshal` of `v`, optional `ext`, then `Header`) for every returned row even though the success path later performs a full decode on needed ledgers. `getTransactionsByLedgerSequence` uses the decoded header mainly for gap detection and decode-error reporting, not for the main transaction-processing loop.

## Anti-Evidence

The partial decode touches only a small prefix of each blob, while the current path still pays much larger costs for copying raw ledger BLOBs into Go, fully decoding used `LedgerCloseMeta` values, hashing envelopes, and serializing the response. Even on skipped ledgers, the extra header parse is likely far smaller than the surrounding I/O and response-generation work.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The partial header decode is real, but it is only a lightweight prefix parse layered on top of much heavier raw-BLOB transfer and downstream full-ledger work. Removing it would clean up wasted instructions, but it is unlikely to deliver a measurable `getTransactions` latency or RPS improvement by itself.

### Lesson Learned

After the recent move from eager full-ledger batch decode to `BatchGetLedgers`, the remaining meaningful wins are in response-size work and transaction lifetime, not in shaving a small prefix parse from each fetched ledger blob.
