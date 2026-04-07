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
