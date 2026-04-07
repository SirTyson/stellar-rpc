# H014: Header-only partial decode before Rust JSON extraction is too small to matter

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Low
**Impact**: CPU overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

The JSON fast path should avoid redundant XDR work before it hands a ledger blob to Rust. If `BatchGetLedgersBySequences()` is doing meaningful duplicate decoding ahead of `lcm_transactions_to_json`, removing that duplicate work should measurably improve `getTransactions`.

## Mechanism

I suspected the partial header decode in `BatchGetLedgersBySequences()` might be a worthwhile target because Go reads the LCM version/ext/header just to obtain sequence and close time, and Rust then reparses the whole blob immediately afterward. If that duplicate parse were large enough, eliminating the Go-side header decode or having Rust return those scalars might reduce per-ledger CPU.

## Trigger

Issue JSON-format `getTransactions` requests that span multiple ledgers and inspect the pre-FFI path inside `BatchGetLedgersBySequences()`.

## Target Code

- `cmd/stellar-rpc/internal/db/ledger.go:BatchGetLedgersBySequences:170-211` — Go performs a small partial XDR decode of each `meta` blob to populate `LedgerMetadataChunk.Header`.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:processChunksJSON:273-288` — JSON path uses the decoded header for `ledgerSeq`/`closeTime` and then calls the Rust full-ledger extractor.

## Evidence

`BatchGetLedgersBySequences()` does call `xdr.Unmarshal` several times per ledger (version, optional ext, header) before returning the chunk (`cmd/stellar-rpc/internal/db/ledger.go:189-208`). `processChunksJSON()` then invokes `LCMTransactionsToJSON(chunk.Lcm)` which reparses the same blob in Rust (`cmd/stellar-rpc/internal/methods/get_transactions.go:287-289`).

## Anti-Evidence

The Go-side work is only a tiny header peel, not a full `LedgerCloseMeta` decode. The dominant cost on this path is still the Rust parse plus per-transaction JSON serialization for result/meta/envelope/events, so removing a few header unmarshals per selected ledger would not move endpoint latency meaningfully.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The duplicated work exists, but it is only a small constant per selected ledger and is swamped by the full per-transaction extraction work that follows.

### Lesson Learned

After the raw-LCM JSON fast path landed, the remaining high-value opportunities are at transaction/page granularity, not in the tiny header bookkeeping that precedes each FFI call.
