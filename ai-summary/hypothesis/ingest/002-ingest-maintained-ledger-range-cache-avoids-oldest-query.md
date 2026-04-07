# H002: Ingest only updates the latest-ledger cache, so every `getTransactions` call still SQL-reads the oldest retained ledger

**Date**: 2026-04-07
**Subsystem**: ingest
**Severity**: Low
**Impact**: fixed DB round-trip / XDR decode overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once tip-following ingest and backfill have established a contiguous retained ledger window, `getTransactions` should be able to validate its requested range from memory without querying SQLite for the oldest retained ledger on every call. The endpoint should pay the actual ledger-fetch cost only for the ledgers it intends to process, not an extra range-discovery query first.

## Mechanism

`writeTx.Commit()` atomically updates only `latestLedgerSeq` and `latestLedgerCloseTime`. As a result, `readTx.GetLedgerRange()` still falls back to `getLedgerRangeWithCache()`, which executes `SELECT meta ... MIN(sequence)` and deserializes the oldest retained `LedgerCloseMeta` on every `getTransactions` request. Because ingest advances the retention window contiguously and trimming is deterministic, it could maintain a lightweight in-memory `LedgerRange` (or a tiny `LedgerBucketWindow[LedgerInfo]`) alongside the existing latest-ledger cache and let `getTransactions` skip that fixed oldest-ledger SQL/XDR step.

## Trigger

1. Run tip-following ingest with a steady retained ledger window.
2. Send many `getTransactions` requests with small limits against already-valid ranges.
3. Compare the current path against a version that serves `GetLedgerRange()` from an ingest-maintained in-memory range cache.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:272-299` — every request opens a read transaction and immediately asks for the ledger range.
- `cmd/stellar-rpc/internal/db/ledger.go:GetLedgerRange:61-66` — read transaction uses only the latest-ledger cache and otherwise falls back to SQL.
- `cmd/stellar-rpc/internal/db/ledger.go:getLedgerRangeWithCache:246-275` — still issues a `MIN(sequence)` lookup and unmarshals the oldest `meta` blob.
- `cmd/stellar-rpc/internal/db/db.go:Commit:301-352` — commit path updates only latest-ledger cache state even though it already knows the retention-window motion.
- `cmd/stellar-rpc/internal/ledgerbucketwindow/ledgerbucketwindow.go:GetLedgerRange:87-104` — existing bounded ledger-info window can materialize first/last ledger info in memory.
- `cmd/stellar-rpc/internal/daemon/daemon.go:220-225` — cache reset boundary after backfill already exists and could reset an expanded range cache too.
- `cmd/stellar-rpc/internal/db/ledger_test.go:BenchmarkGetLedgerRange:187-196` — the repository already benchmarks this path, suggesting its fixed overhead matters.

## Evidence

The code clearly optimizes only half of the range lookup today: latest ledger info is cached, oldest ledger info is not. `getTransactions` always pays that oldest-ledger lookup before doing any request-specific work, even though ingest/backfill are the only writers and they already enforce contiguous progression plus explicit reset points.

## Anti-Evidence

This is a fixed-cost optimization, so it matters most for small requests and high QPS rather than large multi-ledger scans. The cache must be invalidated correctly across restart, backfill, and any future non-contiguous ingest mode, or range validation could drift from the DB.
