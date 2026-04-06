# H002: Pagination Ignores the Existing Transaction Index and Walks Empty Ledgers

**Date**: 2026-04-06
**Subsystem**: methods
**Severity**: High
**Impact**: latency / CPU / DB I/O
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

For cursor-based pagination, `getTransactions` should use the existing transaction index to identify the next `limit` transaction positions after the cursor, then load only the ledgers that actually contain those transactions. A request should not linearly scan every retained ledger just to discover that most of them contribute nothing to the response.

## Mechanism

The handler currently derives the next page by incrementing a TOID cursor and walking ledger-by-ledger until it accumulates enough transactions. But the repository already maintains a `transactions` index with `ledger_sequence` and `application_order`, so the next page could be found with an ordered SQL query over distinct `(ledger_sequence, application_order)` pairs and then hydrated from only the touched ledgers. On sparse windows this should avoid scanning empty ledgers entirely, which is a larger algorithmic win than merely batching the current per-ledger fetches.

## Trigger

1. Backfill a window where many consecutive ledgers are empty or contain only one transaction.
2. Request `getTransactions` from an old `startLedger` or from a cursor just before a long sparse region.
3. Measure latency and ledgers scanned by the current linear walk versus a prototype that pages from the `transactions` table and then loads only referenced ledgers.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:248-276` — pagination is implemented as a linear ledger scan.
- `cmd/stellar-rpc/internal/db/transaction.go:97-121` — ingestion already records `ledger_sequence` and `application_order` for each transaction.
- `cmd/stellar-rpc/internal/db/sqlmigrations/02_transactions.sql:4-10` — the `transactions` table and `ledger_sequence` index already exist.
- `cmd/stellar-rpc/internal/db/transaction.go:194-235` — `GetTransaction` demonstrates using `application_order` to jump directly to a transaction within a ledger.

## Evidence

The codebase already pays ingestion cost to maintain a transaction-position index, but `getTransactions` does not consult it at all. That leaves the endpoint doing algorithmic work proportional to the number of ledgers scanned instead of the number of transactions returned.

## Anti-Evidence

Fee-bump transactions are indexed by both outer and inner hash, so a paging query would need `DISTINCT` or grouping on `(ledger_sequence, application_order)` to avoid double-counting. The response still needs full ledger metadata for hydration, so this does not eliminate ledger decoding entirely; it only narrows the set of ledgers that must be touched.
