# H002: Cursor Pagination Rebuilds the Same Ledger Envelope Map on Every Page

**Date**: 2026-04-07
**Subsystem**: daemon
**Severity**: Medium
**Impact**: repeated hashing / allocation churn
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Sequential `getTransactions` cursor requests that keep walking through the same busy ledger should reuse the already-derived envelope-by-hash mapping for that ledger. Paginating through one ledger in several pages should not re-hash every envelope and rebuild the same map from scratch on each request.

## Mechanism

`transactionsRPCHandler` is effectively stateless, so every request that touches a ledger calls `newLedgerTransactionReader`, which allocates a fresh `map[xdr.Hash]xdr.TransactionEnvelope` and hashes every envelope in `storeTransactions`. The API and integration tests explicitly exercise cursor-based follow-up requests, but there is no hot-ledger cache in the handler or daemon layer. On workloads that page through a single dense ledger with small limits, the daemon pays O(transactions-in-ledger) hashing work on every page instead of once per hot ledger.

## Trigger

Create a ledger with many transactions, then page through it using repeated `getTransactions` calls with `limit=25` or `limit=50`, always feeding the previous response's cursor into the next request. Compare CPU profiles or RPS before and after adding a small LRU keyed by ledger sequence/hash that reuses the parsed envelope map for recently requested ledgers.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:24-31` — handler state contains no cache for parsed hot ledgers.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:289-297` — every request re-enters `processTransactionsInLedger` for each fetched ledger.
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:28-41` — `newLedgerTransactionReader` always allocates a new map and fully initializes reader state.
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:86-103` — `storeTransactions` hashes every envelope in the ledger before any page data is returned.
- `cmd/stellar-rpc/internal/integrationtest/get_transactions_test.go:77-100` — cursor-based follow-up requests are part of the supported access pattern.

## Evidence

The only per-handler memoized field today is the precomputed network ID; there is no structure for retaining parsed ledger readers across requests. `storeTransactions` walks the full envelope list and hashes each entry up front, which means page 2 of the same ledger repeats the exact same work page 1 already did.

## Anti-Evidence

This optimization depends on cross-request locality: if clients mostly ask for one large page or jump across unrelated ledgers, the cache hit rate will be low. It is also a memory-for-CPU tradeoff, so the cache would need a tight size bound to avoid retaining too many large ledgers.
