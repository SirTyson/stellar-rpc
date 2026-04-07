# H002: Read transactions hold SQLite snapshots through CPU-heavy response building

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Medium
**Impact**: lock contention / WAL checkpoint stalls
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

`getTransactions` should keep its SQLite read transaction open only while it is actually reading ledger blobs. Once a batch is materialized into Go memory, the handler should release the snapshot before doing expensive transaction parsing, marshaling, and JSON conversion so concurrent ingestion is not forced to coordinate with CPU-bound response assembly.

## Mechanism

The handler opens a read-only transaction near the top of the request and defers `Done()` until the entire response is built, so the SQLite snapshot survives through `processTransactionsInLedger()`, `db.ParseTransaction()`, base64 encoding, and optional Rust JSON conversion. The write path has WAL auto-checkpointing disabled and instead runs `PRAGMA wal_checkpoint(TRUNCATE)` after every ingest commit; in SQLite WAL mode, long-lived reader snapshots prevent truncate checkpoints from completing cleanly, so concurrent `getTransactions` requests can turn response CPU time into checkpoint latency and writer/read contention.

## Trigger

Run concurrent `getTransactions` requests in `json` format with high limits while ingestion continues to commit new ledgers. Observe request latency and the writer's `wal_checkpoint` timing around commit; long JSON pages should correlate with slower or repeated checkpoint work because the read snapshot remains open after the SQL reads are finished.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:206-215` — read transaction opened once and deferred to function exit
- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:263-299` — batch data is fetched first, then CPU-heavy per-ledger processing happens while the read tx is still open
- `cmd/stellar-rpc/internal/db/ledger.go:ledgerReader.NewTx:155-168` — every request binds a cloned session to a read-only SQLite transaction
- `cmd/stellar-rpc/internal/db/db.go:openSQLiteDB/NewTx/Commit:69-76,231-252,301-352` — WAL auto-checkpointing is disabled and each write runs `PRAGMA wal_checkpoint(TRUNCATE)`

## Evidence

`getTransactionsByLedgerSequence()` opens `readTx` once, defers `Done()`, and then keeps using that same transaction across the whole request (`cmd/stellar-rpc/internal/methods/get_transactions.go:206-215`). The DB configuration explicitly disables SQLite WAL auto-checkpointing and replaces it with a manual truncate checkpoint after every commit (`cmd/stellar-rpc/internal/db/db.go:69-76,241-252`). Because `BatchGetLedgerMetas()` returns fully materialized Go values, the later `processTransactionsInLedger()` work is no longer using SQLite, yet the snapshot still stays open.

## Anti-Evidence

This only matters when `getTransactions` overlaps with ingestion; an isolated request against a quiescent DB will not see much benefit. Any fix must retain snapshot-consistent paging semantics across multi-batch requests, so shortening transaction lifetime may require copying raw blobs or splitting the work into smaller read scopes.
