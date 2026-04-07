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

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Every `getTransactions` call creates a `ledgerReaderTx` via `NewTx` (ledger.go:155-168), which snapshots `cache.latestLedgerSeq` and `cache.latestLedgerCloseTime`. The subsequent `GetLedgerRange` call (ledger.go:61-66) uses the cached latest but has no cached oldest, so it calls `getLedgerRangeWithCache` which executes `SELECT meta FROM ledger_close_meta WHERE sequence = (SELECT MIN(sequence) FROM ledger_close_meta)` and fully deserializes the XDR `LedgerCloseMeta` blob — potentially 50KB–500KB+ — just to extract two integer fields (`LedgerSequence()` and `LedgerCloseTime()`). Both fields are needed: the response populates `OldestLedger` and `OldestLedgerCloseTime` (get_transactions.go:374-375). The `Commit` path (db.go:301-352) only updates the latest-ledger cache fields, never the oldest.

### Code Paths Examined

- `cmd/stellar-rpc/internal/db/ledger.go:NewTx:155-168` — snapshots latest cache into `ledgerReaderTx` but has no oldest cache to snapshot
- `cmd/stellar-rpc/internal/db/ledger.go:GetLedgerRange:61-66` — dispatches to `getLedgerRangeWithCache` when `latestLedgerSeq != 0`
- `cmd/stellar-rpc/internal/db/ledger.go:getLedgerRangeWithCache:246-275` — issues `SELECT meta ... WHERE sequence = (SELECT MIN(sequence) ...)` and full XDR unmarshal of oldest LCM
- `cmd/stellar-rpc/internal/db/db.go:dbCache:49-54` — only has `latestLedgerSeq` and `latestLedgerCloseTime`, no oldest fields
- `cmd/stellar-rpc/internal/db/db.go:Commit:301-352` — updates only `globalCache.latestLedgerSeq/CloseTime` inside lock; calls `trimLedgers` which changes the oldest but never caches the new oldest
- `cmd/stellar-rpc/internal/db/ledger.go:trimLedgers:336-347` — deletes ledgers with `sequence < cutoff` where `cutoff = latestLedgerSeq + 1 - retentionWindow`; the new oldest is deterministically `cutoff`
- `cmd/stellar-rpc/internal/db/db.go:ResetCache:62-67` — clears latest cache; would also need to clear any oldest cache
- `cmd/stellar-rpc/internal/methods/get_transactions.go:370-375` — response uses both `FirstLedger.Sequence` and `FirstLedger.CloseTime`
- `cmd/stellar-rpc/internal/db/sqlmigrations/01_init.sql` — `ledger_close_meta` table has `(sequence INTEGER NOT NULL PRIMARY KEY, meta BLOB NOT NULL)`; close_time is only available inside the `meta` blob

### Findings

1. **The inefficiency is real.** The `dbCache` struct caches the latest ledger info but has no corresponding fields for the oldest. Every `getTransactions` call pays for a full `meta` blob read + XDR deserialization of the oldest LCM just to get `(sequence, close_time)`. The `MIN(sequence)` subquery is O(1) on the primary key B-tree, but reading and unmarshaling the `meta` blob for the oldest ledger is the dominant cost — this blob can be large (all transaction results, events, etc. for that ledger).

2. **The fix is correct and safe.** Adding `oldestLedgerSeq` and `oldestLedgerCloseTime` to `dbCache` follows the exact same RWMutex-guarded pattern as the existing latest fields. At `Commit` time, after `trimLedgers` runs, the new oldest sequence is deterministically `latestLedgerSeq + 1 - retentionWindow` (when trimming occurs). The oldest close time can be fetched from the DB within the same write transaction (one query per commit, ~every 5s) rather than per request. On startup/backfill, `ResetCache` already zeroes the cache, causing fallback to the SQL path until the first commit populates it.

3. **No existing optimization covers it.** There is no pool, LRU, or secondary cache for the oldest ledger info. The only cache is the half-implemented `dbCache` with latest-only fields.

4. **Impact is Low but measurable.** The existing `BenchmarkGetLedgerRange` confirms the team considers this path performance-relevant. The fixed overhead per request is meaningful at high QPS with small limits but is dwarfed by `BatchGetLedgerMetas` for large-limit requests. Conservatively <5% of total request latency for typical workloads, potentially more significant for small-limit high-QPS patterns.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/db/db.go` (add `oldestLedgerSeq` and `oldestLedgerCloseTime` to `dbCache`; update `ResetCache` and `Commit` to manage these fields), `cmd/stellar-rpc/internal/db/ledger.go` (update `NewTx` to snapshot oldest fields, update `GetLedgerRange` / `getLedgerRangeWithCache` to use cached oldest when available, update `ledgerReaderTx` struct)
- **Change description**: Extend `dbCache` with `oldestLedgerSeq`/`oldestLedgerCloseTime`. In `Commit`, after `trimLedgers`, compute the new oldest sequence (`latestLedgerSeq + 1 - retentionWindow` when trimming occurs) and query its close_time from the DB within the write transaction. In `NewTx`, snapshot these into `ledgerReaderTx`. In `GetLedgerRange`/`getLedgerRangeWithCache`, return fully cached `LedgerRange` when both oldest and latest are cached, bypassing the SQL query entirely.
- **Correctness check**: Existing tests — `BenchmarkGetLedgerRange`, `TestGetLedgerRange*`, and integration tests that exercise the `getTransactions` endpoint. Also verify `ResetCache` clears the oldest fields and that the first `getTransactions` after restart falls back to the SQL path until the first commit populates the cache.
- **Benchmark focus**: `BenchmarkGetLedgerRange` should show elimination of the SQL query and XDR deserialization. Expect near-zero ns/op when both cache fields are populated (currently the benchmark should show the SQL+XDR cost). At high concurrency, this also reduces SQLite read contention.
