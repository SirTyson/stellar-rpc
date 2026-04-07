# H002: getTransactions still scans empty ledgers instead of using the transaction index

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: High
**Impact**: DB / CPU / sparse-history scan cost
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

For sparse histories, `getTransactions` should use the existing transaction index to jump directly to the next ledgers and application orders that actually contain transactions. Returning `N` transactions should scale primarily with `N`, not with every ledger sequence between the cursor and the latest retained ledger.

## Mechanism

The current implementation walks ledger ranges in sequence and fetches every `LedgerCloseMeta` batch from `startLedger` to `latestLedger`, even when many of those ledgers contain zero transactions. The DB already maintains a `transactions` table indexed by `ledger_sequence`, and `getTransactionByHash()` proves that the codebase already uses that table to locate precise transactions, so sparse `getTransactions` pages are doing avoidable O(number of ledgers) work where an index-driven plan could identify the next `limit` hits first and fetch only the ledgers that matter.

## Trigger

Load a retention window where only one out of every 10-20 ledgers contains any transactions, then call `getTransactions` from an old `startLedger` with `limit=10`. The handler will batch-read and deserialize a large run of empty ledger metas before it accumulates the first page of actual transactions.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:199-304` — sequential ledger scan from cursor to latest
- `cmd/stellar-rpc/internal/db/ledger.go:BatchGetLedgerMetas:120-139` — fetches every ledger in the scanned range
- `cmd/stellar-rpc/internal/db/transaction.go:getTransactionByHash:200-235` — existing index-driven transaction lookup path
- `cmd/stellar-rpc/internal/db/sqlmigrations/02_transactions.sql:4-10` — `transactions(ledger_sequence, application_order)` storage already exists

## Evidence

The handler never consults the `transactions` table; it only advances `batchStart` ledger-by-ledger (`cmd/stellar-rpc/internal/methods/get_transactions.go:243-294`). Meanwhile the DB schema already stores `(hash, ledger_sequence, application_order)` and has an index on `ledger_sequence` (`cmd/stellar-rpc/internal/db/sqlmigrations/02_transactions.sql:4-10`), and `getTransactionByHash()` already joins that table back to `ledger_close_meta` for targeted retrieval (`cmd/stellar-rpc/internal/db/transaction.go:207-235`).

## Anti-Evidence

If most ledgers in the scanned range already contain transactions, the benefit of an index-driven plan will narrow because the current batched ledger reads stay productive. Any fix must preserve the endpoint's exact cursor semantics and still surface missing-ledger errors consistently when the local store is corrupted.
