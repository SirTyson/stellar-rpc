# H003: Ingest discards fully materialized `LedgerTransaction` objects that the next `getTransactions` request rebuilds

**Date**: 2026-04-07
**Subsystem**: ingest
**Severity**: Medium
**Impact**: CPU / hashing / allocation / hot-ledger first-hit latency
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

The first `getTransactions` request for a freshly closed ledger should be able to reuse the transaction objects ingest already materialized while indexing that ledger. Hot recent requests should not need to rebuild a new ledger transaction reader, rehash every envelope, and reconstruct `ingest.LedgerTransaction` values when ingest produced those same objects moments earlier.

## Mechanism

`transactionHandler.InsertTransactions()` already walks the ledger in application order with `reader.Read()` and obtains full `ingest.LedgerTransaction` values; it then keeps only hash/index data for the DB insert and discards the richer objects. `getTransactions` later calls `newLedgerTransactionReader()`, which eagerly hashes all envelopes into `envelopesByHash` and reconstructs a second stream of `ingest.LedgerTransaction` values before `ParseTransaction()` can run. A bounded ingest-fed recent cache of ordered `[]ingest.LedgerTransaction` keyed by ledger sequence would let the first post-close request skip that entire reconstruction phase and go straight to pagination, `ParseTransaction()`, and output formatting.

## Trigger

1. Run tip-following ingest with dense ledgers.
2. Issue `getTransactions` requests against the newest ledger immediately after close, especially with small limits or multiple concurrent pollers.
3. Compare current latency against a version that checks a recent ingest-populated `[]ingest.LedgerTransaction` cache before calling `newLedgerTransactionReader()`.

## Target Code

- `cmd/stellar-rpc/internal/db/transaction.go:88-110` — `InsertTransactions()` already reads full `ingest.LedgerTransaction` values in order.
- `cmd/stellar-rpc/internal/ingest/service.go:208-224` — ingest commits the ledger, then retains only sequence metadata.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:89-130` — request path rebuilds the transaction stream for every touched ledger.
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:28-104` — `newLedgerTransactionReader()` eagerly rebuilds the envelope-hash map and re-materializes transactions.
- `cmd/stellar-rpc/internal/db/transaction.go:238-277` — `ParseTransaction()` already accepts `ingest.LedgerTransaction`, so a cached slice fits the existing formatter path.

## Evidence

The write path already pays the SDK reader walk and has the exact transaction objects the read path wants. Nothing in `getTransactions` requires those objects to have been rebuilt locally; it only needs application-order `ingest.LedgerTransaction` values to feed into `ParseTransaction()` or direct XDR encoding. Retaining a small recent slice is therefore a true reuse of already-computed work, not a proposal to move new formatting work onto ingest.

## Anti-Evidence

These objects are large and only benefit recent ledgers that are queried while still hot in the cache. JSON formatting, event encoding, and base64 conversion still dominate for some workloads, so the end-to-end gain depends on how much reader reconstruction contributes for dense ledgers.
