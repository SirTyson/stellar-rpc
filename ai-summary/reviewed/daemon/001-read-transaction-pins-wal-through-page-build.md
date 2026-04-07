# H001: getTransactions Holds a SQLite Snapshot Through the Full Page Build

**Date**: 2026-04-07
**Subsystem**: daemon
**Severity**: Medium
**Impact**: DB checkpoint contention / throughput loss
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

`getTransactions` should hold its SQLite read transaction only while it is actively fetching ledger metadata from the database. Once a batch of raw `meta` blobs has been copied into Go memory, the request should release the snapshot so ingestion can continue checkpointing WAL state without waiting for the endpoint's CPU-heavy XDR decode, hashing, base64 encoding, or JSON conversion work.

## Mechanism

`getTransactionsByLedgerSequence` opens a read transaction at the start of the request and defers `readTx.Done()` until after the full response page has been assembled. In this repository, every write commit triggers `PRAGMA wal_checkpoint(TRUNCATE)`, so long-lived reader snapshots can pin WAL state while the handler spends milliseconds-to-tens-of-milliseconds doing non-DB work on already-copied ledger blobs. Under concurrent ingest plus `getTransactions` load, that can turn CPU time in the handler into extra WAL growth, slower checkpoints, and lower endpoint throughput.

## Trigger

Run sustained `getTransactions` traffic with ingestion active, using large pages (`limit=50` or `200`) and especially `format=json`, then compare WAL checkpoint latency, WAL file size, and `getTransactions` p95/p99 after changing the handler to close the read transaction immediately after each `BatchGetLedgers` fetch and reopen only when another batch is needed.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:277-389` — opens `readTx`, defers `Done()`, and keeps the snapshot alive while processing the entire page.
- `cmd/stellar-rpc/internal/db/ledger.go:(ledgerReaderTx).BatchGetLedgers:88-132` — copies each ledger's raw `meta` blob into Go memory, making later per-ledger processing independent of the SQLite transaction.
- `cmd/stellar-rpc/internal/db/db.go:openSQLiteDB:73-80` — configures SQLite in WAL mode with auto-checkpointing disabled.
- `cmd/stellar-rpc/internal/db/db.go:(readWriter).NewTx:239-257` — each write transaction issues `PRAGMA wal_checkpoint(TRUNCATE)` on commit.

## Evidence

The handler starts `readTx` before validation and pagination setup, then keeps it alive until after all ledger chunks have been decoded and converted into `protocol.TransactionInfo` objects. `BatchGetLedgers` returns ordinary Go `[]byte` copies for each ledger meta row, so the long-lived snapshot is not needed to keep the already-fetched batch data valid.

## Anti-Evidence

Changing transaction lifetime must preserve request-start consistency for multi-batch scans, so the fix likely needs per-batch read transactions anchored to the initial ledger-range snapshot rather than a naive early rollback. If the workload has no concurrent ingest or checkpoint pressure, the benefit may be much smaller than under mixed read/write load.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

The `getTransactionsByLedgerSequence` handler opens a read transaction at line 279, defers `Done()` at line 287, then uses it for `GetLedgerRange` (line 290) and a single `BatchGetLedgersBySequences` (line 339). After the batch fetch returns, all ledger metadata is copied into Go `[]byte` slices and the read transaction is never accessed again — yet it remains held throughout the entire XDR deserialization and JSON conversion loop (lines 363-383) until the function returns. The `PRAGMA wal_checkpoint(TRUNCATE)` in `writeTx.Commit` (db.go:252) runs after every ingestion commit and will partially fail if any reader holds a snapshot, preventing WAL truncation.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:279-394` — readTx opened at 279, deferred Done at 287, last use at line 339 (BatchGetLedgersBySequences), processing loop at 363-383 holds it unnecessarily
- `cmd/stellar-rpc/internal/db/ledger.go:BatchGetLedgersBySequences:164-203` — fetches `[][]byte` results via `l.tx.Select()`, returning Go heap copies independent of SQLite pages
- `cmd/stellar-rpc/internal/db/ledger.go:NewTx:219-233` — begins read-only SQL transaction with cached ledger range
- `cmd/stellar-rpc/internal/db/db.go:writeTx.Commit:309-369` — commits write transaction then calls postCommit
- `cmd/stellar-rpc/internal/db/db.go:postCommit:249-259` — issues `PRAGMA wal_checkpoint(TRUNCATE)` after every write commit
- `cmd/stellar-rpc/internal/db/db.go:openSQLiteDB:73-79` — `_wal_autocheckpoint=0` disables automatic checkpointing; system relies entirely on the explicit TRUNCATE pragma
- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:76-234` — CPU-heavy per-transaction XDR decode, JSON/base64 conversion that runs while readTx is held

### Findings

**The inefficiency exists.** After `BatchGetLedgersBySequences` returns at line 339, the read transaction is no longer accessed. All subsequent processing (XDR deserialization at line 369, `processTransactionsInLedger` at line 376 — which includes JSON conversion via xdr2json FFI, base64 encoding, hash computation, and event extraction) operates exclusively on Go heap data. The readTx is held for the entire duration of this CPU work purely because `Done()` is deferred to function return.

**Code has changed from hypothesis description.** The hypothesis describes a multi-batch loop with `batchSize=50`. The current code uses a transaction index (`GetLedgerSequencesWithTransactions` at line 328) to identify only ledgers containing transactions, then makes a single `BatchGetLedgersBySequences` call. This makes the fix significantly simpler than the hypothesis anticipated — no multi-batch consistency concern exists.

**The `transactionReader` query (line 328) is outside the readTx**, using its own DB access via the `TransactionReader` interface. This means even now, there is no cross-query snapshot consistency between the transaction index lookup and the ledger metadata fetch. Releasing the readTx after the batch fetch breaks no additional consistency guarantee.

**WAL pinning is real under concurrent load.** With `_wal_autocheckpoint=0`, the system relies entirely on `PRAGMA wal_checkpoint(TRUNCATE)` after each ingestion commit. If any reader holds a snapshot when the checkpoint runs, SQLite cannot truncate the WAL file. Under sustained concurrent getTransactions load (e.g., 10+ concurrent requests each holding their readTx for 50-200ms of processing time), the probability of overlap with ingestion checkpoints increases substantially. The WAL would grow by the size of each ingestion commit's writes until a checkpoint finally succeeds with no active readers.

**Impact on getTransactions latency is indirect.** The growing WAL doesn't directly slow the current request. However, a persistently large WAL means future reads must check more WAL frames (via the WAL index hash table), adding per-page-lookup overhead. Under sustained load, this creates a feedback loop: long-held readers → WAL growth → slower reads → longer-held readers.

**Severity downgrade from Medium to Low.** The practical impact on getTransactions latency/RPS is likely <5%. The benefit is primarily system-level health: controlled WAL size, reliable checkpoints, and predictable ingestion pipeline latency. The feedback loop described above would require sustained high-concurrency load to produce measurable degradation.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence` — release the readTx immediately after `BatchGetLedgersBySequences` completes (after line 345), before entering the processing loop. The simplest approach: replace the deferred `readTx.Done()` with an explicit call after the batch fetch, keeping the defer as a safety net (idempotent `Done()` is already handled by `Rollback()` returning "not in transaction").
- **Change description**: After line 345 (batch fetch error check), insert `_ = readTx.Done()`. The defer at line 287 remains as a safety net for early-return error paths. This releases the SQLite read snapshot before the CPU-heavy processing loop begins. For the `len(ledgerSeqs) == 0` path, the readTx is released by the defer at function return (negligible hold time).
- **Correctness check**: Existing `getTransactions` tests in `cmd/stellar-rpc/internal/methods/get_transactions_test.go` cover the main code path. The `ledgerRange` variable is already a local copy (not re-queried), and all batch data is in Go memory after the fetch. The `transactionReader` query is independent of `readTx`. No correctness regression expected.
- **Benchmark focus**: Under concurrent load (10+ goroutines calling getTransactions with `limit=200, format=json` while ingestion is active), measure: (1) WAL file size over time, (2) `wal_checkpoint` duration metric, (3) getTransactions p95/p99 latency. Expect WAL size to remain bounded near zero, checkpoint latency to decrease, and a modest (<5%) improvement in tail latency under high concurrency.
