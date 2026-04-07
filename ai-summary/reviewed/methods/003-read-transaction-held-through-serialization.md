# H003: getTransactions Holds the SQLite Read Transaction Open While Doing CPU-Heavy Serialization

**Date**: 2026-04-06
**Subsystem**: methods
**Severity**: Medium
**Impact**: RPS / DB contention
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

`getTransactions` should hold the database read transaction only for the time needed to fetch ledger metadata. Once the relevant ledgers are loaded into memory, the handler should release the SQLite snapshot before doing transaction parsing, base64 encoding, or JSON/FFI conversion so concurrent ingestion is not forced to coexist with a longer-than-necessary read snapshot.

## Mechanism

`getTransactionsByLedgerSequence` opens a read-only `LedgerReaderTx` at the start of the request and defers `Done()` until the full response is built. The same transaction remains open while `processTransactionsInLedger` runs `db.ParseTransaction`, base64 encoding, and the expensive JSON conversion path. Because the database runs in WAL mode, these long-lived read snapshots can keep old pages pinned and stretch writer/checkpoint pressure under overlapping ingest-plus-query workloads even though the DB is no longer needed for the already-fetched ledgers.

## Trigger

1. Ingest new ledgers continuously while sending concurrent `getTransactions` requests, especially `format=json`.
2. Use requests that return enough transactions and events to keep `processTransactionsInLedger` busy after each ledger fetch.
3. Compare write throughput, WAL growth, and request RPS against a version that batches `LedgerCloseMeta` fetches, closes the read transaction, and only then performs serialization.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:218-276` — one read transaction is opened for the whole request and released only after all CPU work is complete.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:144-200` — parsing, base64 encoding, and JSON conversion all happen while that read transaction is still live.
- `cmd/stellar-rpc/internal/db/ledger.go:ledgerReader.NewTx:130-143` — creates the read-only SQLite transaction.
- `cmd/stellar-rpc/internal/db/db.go:openSQLiteDB:69-84` — the DB is explicitly configured in WAL mode, so long reader lifetimes affect WAL/page retention behavior rather than just blocking writes.

## Evidence

The request lifecycle is straightforward in the handler: `readTx` is created at line 221, `Done()` is deferred at line 228, and every ledger's parse/serialize work happens before the defer runs. The CPU-heavy section includes `db.ParseTransaction`, event extraction, `transactionToJSON`, `jsonifySlice`, and `BuildEventsJSONFromTransaction`, all of which occur after the ledger has already been fetched from the DB.

## Anti-Evidence

On an idle node with little or no concurrent ingestion, shortening the read-transaction lifetime may not move single-request latency much. SQLite WAL readers do not hard-block writers, so the benefit should be strongest under mixed read/write load where snapshot lifetime and checkpoint pressure matter.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full request lifecycle in `getTransactionsByLedgerSequence` (lines 218-287). The read transaction is opened at line 221 (`h.ledgerReader.NewTx`), deferred for rollback at line 228, and remains open through the entire ledger iteration loop (lines 258-277). Each iteration calls `fetchLedgerData` (SQL query via `readTx.GetLedger`) then immediately processes transactions in that same iteration via `processTransactionsInLedger`, which performs XDR marshaling, base64 encoding, and — for JSON format — CGo FFI calls to the Rust `xdr_to_json` library. The read transaction is thus held during both DB-fetch and CPU-bound serialization phases, extending the SQLite WAL snapshot lifetime unnecessarily.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:221` — `h.ledgerReader.NewTx(ctx)` opens a read-only SQL transaction
- `cmd/stellar-rpc/internal/methods/get_transactions.go:228-230` — `defer readTx.Done()` keeps the transaction open until the function returns
- `cmd/stellar-rpc/internal/methods/get_transactions.go:258-277` — main loop interleaves `fetchLedgerData` (SQL) with `processTransactionsInLedger` (CPU)
- `cmd/stellar-rpc/internal/methods/get_transactions.go:144-200` — per-transaction serialization: `db.ParseTransaction` (XDR marshal), `transactionToJSON` (3 CGo FFI calls via `xdr2json.ConvertBytes`), `jsonifySlice` (N CGo calls for diagnostic events), `BuildEventsJSONFromTransaction` (N+M CGo calls for contract/transaction events)
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:61-80` — `convertAnyBytes` makes a CGo call to `C.xdr_to_json`, involving `C.CBytes` copy + FFI + `C.GoString` copy back — this is the most expensive per-invocation operation in the JSON path
- `cmd/stellar-rpc/internal/db/ledger.go:130-143` — `NewTx` begins a read-only SQL transaction via `BeginTx` with `sql.TxOptions{ReadOnly: true}`
- `cmd/stellar-rpc/internal/db/db.go:69-75` — DB opened in WAL mode with `_wal_autocheckpoint=0`
- `cmd/stellar-rpc/internal/db/db.go:245` — write path runs `PRAGMA wal_checkpoint(TRUNCATE)` after every ingestion commit — a TRUNCATE checkpoint requires all readers to have released their snapshots

### Findings

The inefficiency is real and the mechanism is correctly identified:

1. **The read transaction is held longer than necessary.** After each `fetchLedgerData` call retrieves the `LedgerCloseMeta` into memory, the DB is not accessed again for that ledger. All subsequent work — `ingest.NewLedgerTransactionReaderFromLedgerCloseMeta`, `db.ParseTransaction` (XDR marshaling), base64 encoding, and especially the JSON-format CGo FFI calls — is pure CPU work on in-memory data. The read transaction serves no purpose during this phase.

2. **The WAL checkpoint mechanism is sensitive to reader lifetime.** The write path explicitly uses `PRAGMA wal_checkpoint(TRUNCATE)`, which is an all-or-nothing checkpoint: it can only fully succeed if no reader holds a snapshot older than the WAL content. Long-lived readers degrade this to a partial checkpoint, causing WAL growth.

3. **The fix is straightforward.** The handler could be restructured to batch-fetch all needed ledger close metas first (the loop at line 258 already fetches one-at-a-time, and `BatchGetLedgers` exists for batch fetching), close the read transaction, then iterate over the in-memory data for serialization. Since the handler needs at most `limit` transactions across typically 1-5 ledgers, memory pressure is negligible.

4. **Severity downgrade rationale.** The hypothesis claims Medium severity, but I assess **Low** (<5% measurable improvement) because: (a) for base64 format, serialization is fast (~1-5ms for 200 transactions), adding only modestly to the DB-fetch time; (b) for JSON format, the FFI calls are slower (~50-200ms total for 200 transactions with events), but the overlap window with ingestion checkpoints (every ~5-6 seconds) is still probabilistically small under moderate concurrency; (c) SQLite handles growing WAL files gracefully — read performance degrades slowly, not abruptly. Under sustained high concurrency (50+ concurrent JSON-format requests), the impact could approach Medium, but that's an atypical workload.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence` (lines 218-287)
- **Change description**: Restructure the main loop into two phases: (1) a fetch phase that iterates ledgers via `readTx.GetLedger`, collecting `xdr.LedgerCloseMeta` values into a slice until enough transactions are available, then closes the read transaction; (2) a processing phase that iterates the collected metas and calls `processTransactionsInLedger` for serialization. The `readTx.Done()` call should be moved from a defer to an explicit call between the two phases. Alternatively, use `readTx.BatchGetLedgers(ctx, startSeq, endSeq)` for the fetch phase — but note that the handler doesn't know the exact end sequence in advance (it depends on transaction density), so either a conservative upper-bound estimate or a one-at-a-time fetch loop is needed.
- **Correctness check**: The existing tests in `cmd/stellar-rpc/internal/methods/get_transactions_test.go` cover pagination, cursor handling, and format output. These should continue to pass since the optimization only changes when the DB transaction is released, not what data is fetched. Cross-ledger consistency within a single response is not required by the API contract (each ledger is independently self-contained).
- **Benchmark focus**: Measure under concurrent load with mixed read/write: run ingestion continuously while sending concurrent `getTransactions` requests with `format=json` and a limit of 200. Compare WAL file size, checkpoint success rate, and p99 request latency before and after. The primary metric should be checkpoint completion rate under load. Single-request latency improvement should be minimal (<1ms); the gain is in system-level throughput under contention.
