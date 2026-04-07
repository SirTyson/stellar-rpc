# H002: Oldest-ledger cache invalidation causes a post-close `getTransactions` miss stampede

**Date**: 2026-04-07
**Subsystem**: ingest
**Severity**: Low
**Impact**: repeated DB range lookups / synchronized cache misses
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once the retention window is advancing contiguously, the oldest retained ledger bound should advance in memory along with the latest bound. After a new ledger is committed, the first burst of `getTransactions` requests should not all rediscover the same oldest ledger by rereading and deserializing the oldest `meta` blob from SQLite.

## Mechanism

When trimming advances the retention window, `writeTx.Commit()` zeroes `oldestLedgerSeq` and `oldestLedgerCloseTime` instead of publishing the new oldest bound. `ledgerReader.NewTx()` snapshots those zero values into each `ledgerReaderTx`, so a burst of concurrent `getTransactions` calls arriving after the close but before the cache is repopulated all fall through to `getLedgerRangeWithCache()` and each issue the same oldest-ledger query plus XDR unmarshal. Because ingest already knows retention moves forward by one contiguous ledger at a time, it could maintain the oldest bound directly (or through a tiny recent-ledger window) and avoid a synchronized miss storm after every trim.

## Trigger

1. Fill the history retention window so each new ledger advances the cutoff.
2. On every new close, fire many concurrent `getTransactions` requests before any prior request has repopulated the oldest-bound cache.
3. Compare current behavior against a version that publishes the new oldest bound during commit instead of invalidating it.

## Target Code

- `cmd/stellar-rpc/internal/db/db.go:350-356` — trim advancement invalidates `oldestLedgerSeq` and `oldestLedgerCloseTime`.
- `cmd/stellar-rpc/internal/db/ledger.go:170-184` — `ledgerReader.NewTx()` snapshots cache state into the read transaction.
- `cmd/stellar-rpc/internal/db/ledger.go:63-80` — `ledgerReaderTx.GetLedgerRange()` falls back when oldest is missing.
- `cmd/stellar-rpc/internal/db/ledger.go:303-329` — `getLedgerRangeWithCache()` rereads and unmarshals the oldest `LedgerCloseMeta`.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:315-326` — `getTransactions` performs this range lookup before any request-specific work.

## Evidence

The current implementation caches the latest bound eagerly but treats the oldest bound as disposable on every trim. Because `NewTx()` snapshots cache values up front, repopulating the global cache in one request does not help sibling requests that already captured zeroes, so concurrent pollers can fan out into identical range queries for the same oldest ledger.

## Anti-Evidence

This only matters once the retention window is full and only for the first post-close burst; requests arriving later reuse the repopulated cache. The absolute cost is also smaller than full ledger processing, so the gain is likely limited to high-QPS small-limit polling patterns.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated in fail/ or success/

### Trace Summary

The `commitAndUpdateCache` function (db.go:340-358) atomically updates the latest-ledger cache but zeroes the oldest cache when trim advances the retention window. Every subsequent `getTransactions` call creates a `ledgerReaderTx` via `NewTx` (ledger.go:170-183) that snapshots these zeroed values. The snapshotted `GetLedgerRange` (ledger.go:63-80) then falls through to `getLedgerRangeWithCache` (ledger.go:303-329), which issues `SELECT meta FROM ledger_close_meta WHERE sequence = (SELECT MIN(sequence) FROM ledger_close_meta)` and fully deserializes the XDR blob (potentially 50KB–500KB+) just to extract two integer fields. Crucially, the Tx path does NOT write back to the global cache — only non-Tx callers like `getHealth` and `getEvents` (ledger.go:264-276) repopulate it.

### Code Paths Examined

- `cmd/stellar-rpc/internal/db/db.go:49-56` — `dbCache` struct has `oldestLedgerSeq` and `oldestLedgerCloseTime` fields (they exist, contrary to some analyses)
- `cmd/stellar-rpc/internal/db/db.go:340-358` — `commitAndUpdateCache` updates latest cache eagerly but ZEROES oldest on trim instead of setting new value
- `cmd/stellar-rpc/internal/db/db.go:350-356` — invalidation: `oldestLedgerSeq = 0` when `oldestLedgerSeq < cutoff` where `cutoff = ledgerSeq + 1 - retentionWindow`
- `cmd/stellar-rpc/internal/db/ledger.go:170-183` — `NewTx` snapshots all cache fields under RLock; zeroed oldest propagates to read transaction
- `cmd/stellar-rpc/internal/db/ledger.go:63-80` — `ledgerReaderTx.GetLedgerRange`: when `latestLedgerSeq != 0 && oldestLedgerSeq == 0`, falls through to `getLedgerRangeWithCache`
- `cmd/stellar-rpc/internal/db/ledger.go:303-329` — `getLedgerRangeWithCache`: reads full `meta` blob via `SELECT meta ... WHERE sequence = (SELECT MIN(sequence) ...)`, deserializes entire `xdr.LedgerCloseMeta` for just `LedgerSequence()` + `LedgerCloseTime()`
- `cmd/stellar-rpc/internal/db/ledger.go:241-291` — non-Tx `GetLedgerRange` DOES repopulate global cache (lines 270-275), but `getTransactions` never calls this path
- `cmd/stellar-rpc/internal/db/ledger.go:390-402` — `trimLedgers` deletes `sequence < cutoff`; after trim, oldest is deterministically `cutoff`
- `cmd/stellar-rpc/internal/methods/get_transactions.go:312-326` — `fetchLedgerMetas` uses Tx path exclusively: `NewTx` → `readTx.GetLedgerRange`

### Findings

1. **Cache invalidation is confirmed.** `commitAndUpdateCache` (db.go:353-356) zeroes `oldestLedgerSeq` when trim advances the retention window. The cache fields exist in `dbCache` but get invalidated rather than updated to the new oldest value.

2. **The Tx path never repopulates the global cache.** `ledgerReaderTx.GetLedgerRange` has no reference to the global cache — it returns a local result from the DB query without writing back. Only the non-Tx `ledgerReader.GetLedgerRange` (lines 270-275) repopulates. Since `getTransactions` exclusively uses the Tx path, it NEVER repopulates the oldest cache itself.

3. **The stampede window is real but bounded.** After each cache invalidation, all concurrent Tx-path requests that snapshotted `oldestLedgerSeq = 0` will independently hit the DB. Non-Tx callers (`getHealth` at get_health.go:22, `getEvents` at get_events.go:128, `getFeeStats` at get_fee_stats.go:41) repopulate the cache, limiting the window. However, in a pure `getTransactions` workload with no other endpoints being called, the cache stays at 0 permanently — every request hits the DB.

4. **The per-miss cost is non-trivial.** `getLedgerRangeWithCache` fetches the entire `meta` blob (which includes all transaction results, events, state changes for that ledger) and deserializes it via XDR just to extract a 4-byte sequence and 8-byte timestamp. This is 0.5–2ms per request depending on ledger size.

5. **The fix is straightforward.** Since `trimLedgers` deletes `sequence < cutoff` and the retention window is contiguous, after commit the new oldest is deterministically `cutoff`. The commit path could set `oldestLedgerSeq = cutoff` and query the close time from the DB within the same write transaction (one query per commit, ~every 5s). This eliminates the per-request fallback entirely.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/db/db.go` — in `commitAndUpdateCache`, replace the zeroing of `oldestLedgerSeq/CloseTime` with setting them to the new oldest values. Compute `cutoff = ledgerSeq + 1 - retentionWindow`, set `oldestLedgerSeq = cutoff`, and either query the close time from the DB within the write transaction or read it from the `LedgerCloseMeta` that's being trimmed to.
- **Change description**: In `commitAndUpdateCache`, after `trimLedgers` has executed, instead of `w.globalCache.oldestLedgerSeq = 0`, set `w.globalCache.oldestLedgerSeq = cutoff`. For close time, add a helper query within the write transaction: `SELECT meta FROM ledger_close_meta WHERE sequence = ?` with `cutoff` as parameter, then extract `LedgerCloseTime()`. Alternatively, if the schema is extended with a `close_time` column, read that directly (much cheaper).
- **Correctness check**: Existing tests — `TestGetLedgerRange*`, `BenchmarkGetLedgerRange`, and integration tests exercising `getTransactions`. Verify `ResetCache` still works (it already zeroes oldest fields). Verify that after restart, the first commit correctly sets the oldest cache.
- **Benchmark focus**: `BenchmarkGetLedgerRange` should show near-zero cost when both cache fields are populated (eliminating the SQL + XDR deserialization). Under concurrent load, measure reduction in SQLite read contention on the `ledger_close_meta` table during the post-commit window. Expect <5% latency improvement for typical `getTransactions` calls, more for small-limit high-QPS patterns.
