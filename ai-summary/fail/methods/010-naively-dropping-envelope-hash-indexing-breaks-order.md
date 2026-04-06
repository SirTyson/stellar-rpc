# H010: Naively Dropping Envelope Hash Indexing Would Break Transaction Ordering

**Date**: 2026-04-06
**Subsystem**: methods
**Severity**: Medium
**Impact**: CPU / allocations
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Any optimization that removes reader setup work for non-empty ledgers must still return transactions in application order with the correct envelope matched to the correct result and metadata entry. A faster path is only valid if it preserves the exact transaction ordering semantics of `getTransactions`.

## Mechanism

I investigated whether `getTransactions` could avoid the SDK reader's envelope-hash map for dense ledgers and simply iterate `TransactionEnvelopes()` alongside `TxProcessing`. The actual ordering model defeats that: `LedgerCloseMeta.TransactionEnvelopes()` flattens tx-set phases and clusters, while transaction results/meta are exposed in processing order via `TransactionHash(i)` and `TxProcessing`. The SDK reader hashes envelopes first precisely because those orders diverge, so a generic "skip the hash map for non-empty ledgers" optimization would mis-pair envelopes and results.

## Trigger

1. Take any ledger where tx-set order differs from processing order.
2. Replace `NewLedgerTransactionReaderFromLedgerCloseMeta` with naive sequential iteration over `TransactionEnvelopes()`.
3. Observe mismatched envelope/result pairs or out-of-order transactions in `getTransactions`.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:104-145` — `getTransactions` relies on the SDK reader for envelope/result/meta alignment.
- `github.com/stellar/go-stellar-sdk/ingest/ledger_transaction_reader.go:123-149` — SDK comment and implementation explain why envelopes are hashed before reads.
- `github.com/stellar/go-stellar-sdk/xdr/ledger_close_meta.go:62-98` — envelope enumeration order and processing-order hash access come from different data paths.

## Evidence

The SDK reader explicitly documents that "envelopes in the meta ... are not in the same order as the actual list of metas," then hashes every envelope to associate it with `TransactionHash(i)`. `LedgerCloseMeta.TransactionEnvelopes()` also builds its result by flattening phases/components/clusters, which is not the same API used to access processing-order results and metas.

## Anti-Evidence

If extra persisted ordering metadata existed, a specialized fast path might become viable. But within the current schema and data model, the generic idea of removing the hash-indexing step is not correct.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-06
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The envelope hash map is not just an implementation detail; it is the correctness bridge between tx-set envelope order and processing-order transaction metadata. Without additional stored ordering information, dropping it would return the wrong transactions.

### Lesson Learned

When a hot path looks expensive, first verify whether the work is compensating for an ordering or identity mismatch elsewhere in the data model. In `getTransactions`, the costly envelope hashing is only safely skippable in narrow cases where the handler can prove it will return zero transactions from the ledger.
