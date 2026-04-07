# H002: Missing Tx-Scoped Ledger Streaming Keeps `getTransactions` Waiting for Full Batch Decode

**Date**: 2026-04-07
**Subsystem**: methods
**Severity**: Medium
**Impact**: latency / CPU / allocations
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

After opening a read snapshot, `getTransactions` should be able to stream ledgers in sequence order and stop immediately once the page is full. The first returned transaction should not have to wait for the DB layer to scan and unmarshal an entire batch of `LedgerCloseMeta` rows into memory first.

## Mechanism

The non-transactional `LedgerReader` already exposes `StreamLedgerRange`, but `LedgerReaderTx` has no tx-scoped streaming equivalent. Because `getTransactions` must keep one read snapshot for both `GetLedgerRange` and the ledger walk, it cannot use the streaming API and instead falls back to `BatchGetLedgerMetas`, which calls `Select` into `[]xdr.LedgerCloseMeta` and returns only after every row in the range has been decoded. That front-loads XDR unmarshal work, delays time-to-first-transaction, and keeps unused tail ledgers alive on the heap until the whole batch is materialized.

## Trigger

1. Use ledgers with large `LedgerCloseMeta` blobs (for example, ledgers carrying many events).
2. Issue `getTransactions` requests that usually fill from the first few ledgers in the scanned range.
3. Compare latency and peak heap against a version that adds `StreamLedgerRange` to `LedgerReaderTx` and processes each row as it is scanned.

## Target Code

- `cmd/stellar-rpc/internal/db/ledger.go:25-40` — `LedgerReader` supports `StreamLedgerRange`, but `LedgerReaderTx` does not.
- `cmd/stellar-rpc/internal/db/ledger.go:118-140` — `BatchGetLedgerMetas` materializes the full batch into `[]xdr.LedgerCloseMeta`.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:263-299` — the handler cannot start parsing transactions until the full batch has been returned.

## Evidence

The interface mismatch is explicit: the handler opens a read transaction specifically to preserve a single snapshot, then loses access to the only range-streaming primitive and is forced into eager batch materialization. Unlike a streaming cursor, the current code cannot overlap row scanning with transaction parsing or stop the DB decode early once `done` flips true.

## Anti-Evidence

If the request usually consumes the whole batch anyway, streaming mainly improves memory shape and time-to-first-byte rather than total work. Some SQL drivers already stream rows internally, so the biggest gain depends on how much of the current cost comes from `[]xdr.LedgerCloseMeta` heap materialization rather than the underlying SQLite scan itself.
