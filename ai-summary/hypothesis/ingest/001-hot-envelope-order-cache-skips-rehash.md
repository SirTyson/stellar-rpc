# H001: Hot recent ledgers still rebuild envelope order even though ingest already walked every transaction

**Date**: 2026-04-07
**Subsystem**: ingest
**Severity**: Medium
**Impact**: CPU / envelope hashing / allocation overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When `getTransactions` serves a recently ingested ledger, especially with a small `limit` or a cursor deep into the ledger, it should not need to hash every envelope in that ledger before it can return the first requested transaction. The hot path should be able to reuse the application-order envelope mapping that ingest already discovered while indexing that same ledger.

## Mechanism

`newLedgerTransactionReader()` eagerly calls `storeTransactions()`, which hashes every envelope and builds `envelopesByHash` for the entire ledger before `Seek()` or `Read()` can return anything. `getTransactions` pays that O(txCount) setup cost on every request touching the ledger, even when the request only needs one or two transactions. During ingest, `InsertTransactions()` already walks the ledger sequentially and computes the same transaction hashes and application-order information to populate the lookup table, so an ingest-populated recent ordered-envelope cache (for example `[]xdr.TransactionEnvelope` keyed by ledger sequence) could let hot requests skip the full rehash/map-build phase.

## Trigger

1. Ingest a run of dense recent ledgers.
2. Call `getTransactions` against the newest ledger with `limit=1..5`, or with a cursor late in the ledger.
3. Compare the current path against a version that reuses an ingest-populated ordered-envelope slice for recent ledgers before falling back to `newLedgerTransactionReader()`.

## Target Code

- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:newLedgerTransactionReader:28-41` — request path allocates a fresh reader and eagerly builds per-ledger envelope state.
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:storeTransactions:86-104` — hashes every envelope up front to populate `envelopesByHash`.
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:hashTransactionInEnvelopeWithID:111-160` — per-envelope hashing work repeated for every request.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:88-129` — request path constructs the reader before it knows how many transactions it will actually return.
- `cmd/stellar-rpc/internal/db/transaction.go:InsertTransactions:68-143` — ingest already performs the sequential transaction walk and computes outer/inner hashes plus application order.
- `cmd/stellar-rpc/internal/ledgerbucketwindow/ledgerbucketwindow.go:NewLedgerBucketWindow/Get/GetLedgerRange:25-120` — existing bounded contiguous-ledger window suitable for a recent ordered-envelope cache.

## Evidence

The hot request path always rebuilds `envelopesByHash` from scratch, and it does so before `Seek()` can skip ahead to the requested cursor. In parallel, ingest already pays for a full ordered transaction walk in `InsertTransactions()` to compute the transaction-index data that powers `getTransaction` lookup, so the system has a natural place to retain a lightweight recent-ledger ordering artifact without adding a new full-ledger scan.

## Anti-Evidence

This only helps recently ingested ledgers and mostly when ledgers are dense enough that envelope hashing is a noticeable share of request time. It does not remove the later `ParseTransaction()` / event / encoding costs, so the end-to-end win is bounded unless it is paired with other hot-ledger caching.
