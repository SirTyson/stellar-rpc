# H003: The custom reader still flattens envelopes into a throwaway slice before building the hash map

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Low
**Impact**: allocation / CPU overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When `getTransactions` builds `envelopesByHash`, it should walk the transaction-set phases directly and hash envelopes into the map in one traversal. It should not first allocate a temporary `[]TransactionEnvelope` that exists only to be iterated once and discarded.

## Mechanism

`storeTransactions()` calls `reader.lcm.TransactionEnvelopes()`, and that helper allocates a new slice and appends every envelope from the generalized transaction-set phases into it. `storeTransactions()` then immediately loops over that flattened slice, hashes each envelope, and copies the envelope into `envelopesByHash`. On dense ledgers this creates a pure throwaway allocation and an extra envelope traversal on every request; because the local reader already owns the envelope-walk logic, it could inline the phase/stage/cluster traversal and hash directly into the map.

## Trigger

Use `getTransactions` on dense ledgers with many envelopes per ledger, especially small-limit polling where reader setup dominates useful work. Allocation profiles should show `LedgerCloseMeta.TransactionEnvelopes()` creating a transient envelope slice during every `newLedgerTransactionReader()` call.

## Target Code

- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:storeTransactions:86-103` — hashes envelopes from a one-shot flattened slice
- `github.com/stellar/go-stellar-sdk/xdr/ledger_close_meta.go:LedgerCloseMeta.TransactionEnvelopes` — allocates and fills a fresh `[]TransactionEnvelope` before the local code hashes it

## Evidence

The local reader's `storeTransactions()` ranges over `reader.lcm.TransactionEnvelopes()` (`cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:88-93`). The SDK helper's implementation flattens phases/components/stages into `envelopes := make([]TransactionEnvelope, 0, l.CountTransactions())` and returns that fresh slice before the caller does any useful work. A temporary measurement shows `xdr.TransactionEnvelope` is 32 bytes, so a 200-transaction ledger already creates at least ~6.4KB of short-lived envelope headers before map storage and hashing even start.

## Anti-Evidence

This does not remove the dominant work of hashing every envelope or building the map itself, so the win is inherently smaller than caching the hash map across requests. The temporary slice stores only envelope headers, not full deep copies of the underlying transaction data, which keeps the likely improvement in the low but measurable range.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated (related to reviewed H004 but targets a different micro-optimization)
**Failed At**: reviewer

### Trace Summary

Traced `storeTransactions` (ledger_transaction_reader.go:86-103) through `TransactionEnvelopes()` (go-stellar-sdk xdr/ledger_close_meta.go:62-93). For V1/V2 LCMs, the SDK allocates a flat slice (`make([]TransactionEnvelope, 0, CountTransactions())`), iterates phases/components/stages to fill it, and returns it. The local `storeTransactions` then iterates that slice to hash each envelope. For V0 LCMs, `TransactionEnvelopes()` returns the existing `TxSet.Txs` slice directly — no allocation at all.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:storeTransactions:86-103` — iterates `TransactionEnvelopes()` result, hashes each envelope via `hashTransactionInEnvelopeWithID`
- `go-stellar-sdk@v0.4.0/xdr/ledger_close_meta.go:62-93` — V0 returns existing slice (line 66, no alloc); V1/V2 allocate a new slice and flatten phases/stages/clusters
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:111-161` — `hashTransactionInEnvelopeWithID` performs XDR marshal + SHA256 per envelope (~6-20µs each)

### Why It Failed

The throwaway slice allocation is negligible compared to the per-envelope hashing work it feeds. For 200 transactions, the slice costs ~6.4KB + ~1µs (allocation + copy of shallow struct headers). The hashing work on the same 200 envelopes costs ~2.6ms (XDR marshal + SHA256 per envelope). The slice overhead is <0.04% of the hash-map-building cost.

The hypothesis also incorrectly claims an "extra envelope traversal." The total number of envelope visits is identical whether you traverse phases→slice→hash or traverse phases→hash-inline: each envelope is visited exactly twice (once to collect, once to hash) vs once (to hash during collection). Eliminating one pass over 200 small structs saves ~100ns — far below noise.

Additionally, fixing this requires duplicating the SDK's multi-version phase/stage/cluster traversal logic locally, creating fragile coupling to SDK internals that could change across versions. The already-reviewed H004 (envelope hash map caching) would eliminate this allocation entirely as a side effect for cached ledgers, making this a strictly dominated optimization.

### Lesson Learned

Micro-allocations of small struct-header slices (<10KB) that feed into much heavier per-element work (XDR marshal + crypto hash) are not worth optimizing independently. Focus on eliminating the dominant per-element cost or caching the final result instead.
