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
