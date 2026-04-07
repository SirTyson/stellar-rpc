# H003: The Indexed Planner Still Pays a Second SQL Round-Trip Outside the Read Snapshot

**Date**: 2026-04-07
**Subsystem**: methods
**Severity**: Medium
**Impact**: latency / DB I/O / wasted retries under ingest
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

After `getTransactions` opens a read transaction, all planning and ledger fetch work should execute against that same snapshot. The handler should not need one query against the global DB handle to find ledger sequences and a second query inside `readTx` to fetch the corresponding metadata.

## Mechanism

`getTransactionsByLedgerSequence()` opens `readTx`, but the transaction-index planner runs through the handler-wide `transactionReader`, which was constructed from the top-level `db.SessionInterface` rather than the read transaction. The request therefore pays two SQL round-trips and materializes an intermediate `[]uint32` ledger list before building a second `IN (...)` query. Under concurrent ingestion, the planner can also observe newer transaction rows than the already-open `readTx` snapshot, which turns the missing-ledger validation into wasted failing work. A tx-scoped planner query or a single joined query inside `LedgerReaderTx` would remove both the extra round-trip and the snapshot mismatch.

## Trigger

1. Run continuous ingestion while issuing `getTransactions` requests against recent ledgers.
2. Observe requests that plan against one snapshot and fetch against an older one, especially around ledger boundaries.
3. Compare latency and failure rate against a version that fetches selected `sequence, meta` rows from the transaction index and `ledger_close_meta` in one read-transaction query.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:254-316` — opens `readTx`, then calls the global `transactionReader` before issuing a second fetch query through `readTx`.
- `cmd/stellar-rpc/internal/db/transaction.go:60-70` — `transactionHandler` stores a standalone `db.SessionInterface`, not a tx-scoped reader.
- `cmd/stellar-rpc/internal/db/transaction.go:GetLedgerSequencesWithTransactions:170-187` — planner query runs on that standalone DB session.
- `cmd/stellar-rpc/internal/db/ledger.go:NewTx:219-233` — the ledger fetch happens inside a separate read-only snapshot.
- `cmd/stellar-rpc/internal/db/ledger.go:BatchGetLedgersBySequences:164-204` — second query reconstructs the selected ledgers via an `IN (...)` list.

## Evidence

The handler deliberately keeps a read transaction open to hold one ledger snapshot, but the planner side-step defeats that design. Because the planner and fetch phases are separated by an intermediate slice of ledger sequences, the code also has to build and validate a second query result set even though SQLite could resolve the same selection directly with a joined, ordered read inside `readTx`.

## Anti-Evidence

On an idle node with no concurrent ingest, the mismatch manifests only as an extra query and some slice/SQL-string construction, so the gain will be smaller. The strongest benefit appears under sustained read/write overlap, where eliminating planner/fetch skew also prevents spurious retry-worthy failures.
