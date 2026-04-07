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

---

## Review

**Verdict**: VIABLE
**Severity**: Informational
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

The read transaction opened at `get_transactions.go:206` (`readTx`) is used for three SQL operations: `GetLedgerRange` (line 217), and `BatchGetLedgerMetas` in the batch loop (line 263). After each `BatchGetLedgerMetas` call returns fully materialized `[]xdr.LedgerCloseMeta` values, the subsequent `processTransactionsInLedger` loop (line 290) performs expensive CPU-only work — `ingest.NewLedgerTransactionReaderFromLedgerCloseMeta`, `db.ParseTransaction`, and for JSON format, three `xdr2json.ConvertBytes` FFI calls per transaction plus diagnostic event conversion — all while the SQLite snapshot remains open. The write path confirms: `wal_autocheckpoint=0` (db.go:75) and `PRAGMA wal_checkpoint(TRUNCATE)` after every commit (db.go:244).

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:206-215` — readTx opened once, deferred Done() at function exit; snapshot lives for the entire request
- `cmd/stellar-rpc/internal/methods/get_transactions.go:247-301` — batch loop: `BatchGetLedgerMetas` (SQL read) then `processTransactionsInLedger` (pure CPU) per batch, readTx stays open across all iterations
- `cmd/stellar-rpc/internal/methods/get_transactions.go:75-199` — `processTransactionsInLedger`: creates `LedgerTransactionReader` from in-memory LCM, parses transactions, performs JSON FFI conversion — no SQL access
- `cmd/stellar-rpc/internal/methods/json.go:12-37` — `transactionToJSON`: three `xdr2json.ConvertBytes` FFI calls per transaction (result, envelope, meta)
- `cmd/stellar-rpc/internal/db/ledger.go:155-168` — `NewTx`: clones session, begins read-only SQLite transaction, snapshots cache under RLock
- `cmd/stellar-rpc/internal/db/ledger.go:120-140` — `BatchGetLedgerMetas`: SQL query returns fully deserialized `[]xdr.LedgerCloseMeta` — data is fully in Go memory after return
- `cmd/stellar-rpc/internal/db/db.go:69-76` — `_wal_autocheckpoint=0` disables auto-checkpointing
- `cmd/stellar-rpc/internal/db/db.go:241-252` — `postCommit` runs `PRAGMA wal_checkpoint(TRUNCATE)` after every write commit

### Findings

The inefficiency is structurally real: after `BatchGetLedgerMetas` returns, the read transaction provides no value — all data is materialized in Go memory — yet the snapshot prevents `wal_checkpoint(TRUNCATE)` from completing if it overlaps with an ingestion commit. However, the practical impact is minimal for several reasons:

1. **Checkpoint is non-blocking.** `PRAGMA wal_checkpoint(TRUNCATE)` returns immediately, checkpointing as many frames as possible. It does not block or stall the writer. The only consequence of an open reader is that the WAL file is not fully truncated.

2. **WAL growth is bounded and self-correcting.** An incomplete truncation leaves at most one ledger's worth of WAL pages. The next checkpoint (~5 seconds later) will truncate them if no reader is open. The WAL-index (shared memory hash table) provides O(1) page lookups regardless of WAL size, so read performance is unaffected.

3. **Overlap probability is low.** Ingestion commits once per ledger close (~5-6 seconds). A typical getTransactions request with default limits runs for 10-200ms. The probability that a request's snapshot overlaps with a commit checkpoint is roughly 2-4% under normal load.

4. **No reader/writer lock contention.** SQLite WAL mode is specifically designed so readers never block writers and vice versa. The hypothesis's claim of "lock contention" is inaccurate — the only effect is WAL file size management.

5. **Correctness complicates the fix.** The read transaction provides snapshot consistency across batch boundaries (lines 247-301). Without it, a concurrent writer could trim ledgers between batches, causing "database does not contain metadata for ledger" errors at line 281. A correct fix would need to either pre-fetch all needed ledgers (increasing memory, and the ledger count is unknown upfront since the limit is on transactions) or accept weaker consistency guarantees.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence` — restructure the batch loop to release and re-acquire readTx between batch fetch and batch processing, or pre-fetch all batches then close readTx before processing
- **Change description**: After all `BatchGetLedgerMetas` reads complete for a given batch, close the read transaction before calling `processTransactionsInLedger`. For multi-batch requests, re-open the read transaction for the next batch. Alternatively, pre-compute the number of ledgers needed and fetch them all in one read scope.
- **Correctness check**: Existing tests in `cmd/stellar-rpc/internal/methods/get_transactions_test.go` cover pagination and range validation; verify no test fails when the snapshot is released between batches
- **Benchmark focus**: Measure `wal_checkpoint` duration (already instrumented in `durationMetrics["wal_checkpoint"]`) under concurrent getTransactions+ingestion load. Expected improvement: negligible under normal load; potentially measurable only under sustained high-concurrency JSON format requests

---

## PoC Attempt

**Result**: POC_PASS
**Date**: 2026-04-07
**PoC by**: claude-opus-4.6, high

### Changes Made

- `cmd/stellar-rpc/internal/methods/get_transactions.go:339-343` — Added explicit early `readTx.Done()` call after `BatchGetLedgersBySequences` and chunk verification complete, but before the CPU-heavy transaction processing loop. The existing `defer readTx.Done()` (line 261-263) remains as a safety net for early error returns and the empty-results path.

### Demonstration

The optimization releases the SQLite read-only snapshot immediately after all needed ledger data is materialized into Go memory (`chunks` slice), before the CPU-heavy processing loop that performs XDR unmarshaling, transaction parsing, base64 encoding, and optional Rust FFI JSON conversion. This prevents the snapshot from blocking `PRAGMA wal_checkpoint(TRUNCATE)` during concurrent ingestion commits. The change is safe because `Done()` calls `Rollback()` which is idempotent — the deferred call becomes a no-op.

### Test Results

All 8 getTransactions unit tests pass (TestGetTransactions_DefaultLimit, TestGetTransactions_DefaultLimitExceedsLatestLedger, TestGetTransactions_CustomLimit, TestGetTransactions_CustomLimitAndCursor, TestGetTransactions_InvalidStartLedger, TestGetTransactions_LedgerNotFound, TestGetTransactions_LimitGreaterThanMaxLimit, TestGetTransactions_InvalidCursorString, TestGetTransactions_JSONFormat, TestGetTransactions_NoResults). All 17 test packages across `cmd/stellar-rpc/internal/...` pass with `-race` enabled.
