# H001: Full-range cache avoids decoding the oldest retained ledger on every request

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Low
**Impact**: DB / XDR decode / allocation overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

`getTransactions` should answer the retained ledger window metadata from an in-memory range cache, or at worst from a header-only read. A steady-state request with a warm DB should not fully deserialize the oldest retained `LedgerCloseMeta` blob just to populate `oldestLedger`, `oldestLedgerCloseTimestamp`, and request validation bounds.

## Mechanism

`getTransactionsByLedgerSequence()` always calls `readTx.GetLedgerRange()` before it scans any ledgers. Even when the latest ledger is cached, `getLedgerRangeWithCache()` still executes `SELECT meta ... MIN(sequence)` and scans the row into `[]xdr.LedgerCloseMeta`, which fully unmarshals the oldest retained LCM just to read its sequence and close time. The write path already knows the retention cutoff at commit time, so extending the cache to track the first retained ledger (or reusing the existing header-only decode path) should eliminate this unconditional per-request decode.

## Trigger

Run repeated `getTransactions` tip-polling requests with a small limit (`1-10`) against a warm retained window. A CPU/alloc profile should show `GetLedgerRange()` work before any page-specific ledger scan, even when the returned page is satisfied by the first current ledger.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:206-233` - every request resolves the ledger range before pagination and scanning
- `cmd/stellar-rpc/internal/db/ledger.go:ledgerReaderTx.GetLedgerRange:61-65` - read transaction always routes through the range helpers
- `cmd/stellar-rpc/internal/db/ledger.go:getLedgerRangeWithCache:246-275` - cached path still selects and fully scans the oldest `meta` blob
- `cmd/stellar-rpc/internal/db/db.go:writeTx.Commit:301-352` - commit already computes retention trimming and updates cache under lock

## Evidence

The handler opens a read transaction and immediately calls `GetLedgerRange()` before it has processed any ledger data (`cmd/stellar-rpc/internal/methods/get_transactions.go:206-233`). The "cached" range path in `ledger.go` only avoids fetching the latest ledger; it still runs `sq.Select("meta")` for `MIN(sequence)` and scans into `[]xdr.LedgerCloseMeta`, which forces a full XDR unmarshal of the oldest retained blob (`cmd/stellar-rpc/internal/db/ledger.go:246-275`). There is already a dedicated benchmark for this path (`cmd/stellar-rpc/internal/db/ledger_test.go:187-197`), and the DB write path already has the information needed to advance the retained-window head during trim (`cmd/stellar-rpc/internal/db/db.go:301-352`).

## Anti-Evidence

If a request already scans and deserializes many ledgers or transactions, one extra oldest-ledger decode will be amortized. Any fix must preserve snapshot-consistent range reporting when the retention window advances, especially across startup cache misses and post-trim commits.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS - not previously investigated

### Trace Summary

Traced from `getTransactionsByLedgerSequence` (get_transactions.go:206) through `ledgerReaderTx.GetLedgerRange` (ledger.go:61) into `getLedgerRangeWithCache` (ledger.go:246-275). Confirmed the "cached" path only avoids the latest-ledger query - it still executes `SELECT meta FROM ledger_close_meta WHERE sequence = (SELECT MIN(sequence) FROM ledger_close_meta)` and scans the full XDR blob into `[]xdr.LedgerCloseMeta`, fully deserializing all transaction data, results, and meta just to extract `LedgerSequence()` and `LedgerCloseTime()`. The `dbCache` struct (db.go:49-54) holds only `latestLedgerSeq` and `latestLedgerCloseTime`, with no oldest-ledger fields. The `ledger_close_meta` table schema (01_init.sql:14-17) has only `sequence INTEGER` and `meta BLOB` columns - close time is not denormalized, so any read of oldest close time currently requires blob deserialization.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:206-217` - every request creates a read tx and calls `GetLedgerRange()` before any data processing
- `cmd/stellar-rpc/internal/db/ledger.go:ledgerReaderTx.GetLedgerRange:61-65` - delegates to `getLedgerRangeWithCache` when cache has latest ledger
- `cmd/stellar-rpc/internal/db/ledger.go:getLedgerRangeWithCache:246-275` - selects full `meta` blob for MIN(sequence), scans into `[]xdr.LedgerCloseMeta` (full XDR deserialization), only uses `.LedgerSequence()` and `.LedgerCloseTime()`
- `cmd/stellar-rpc/internal/db/db.go:dbCache:49-54` - only caches `latestLedgerSeq` and `latestLedgerCloseTime`, no oldest-ledger fields
- `cmd/stellar-rpc/internal/db/db.go:writeTx.Commit:301-353` - updates cache atomically with commit under Lock; trim cutoff is `latestSeq + 1 - retentionWindow`, oldest ledger info could be updated here
- `cmd/stellar-rpc/internal/db/ledger.go:ledgerReader.NewTx:155-168` - snapshots cache under RLock into `ledgerReaderTx` fields; would need to also snapshot oldest-ledger fields
- `cmd/stellar-rpc/internal/db/sqlmigrations/01_init.sql:14-17` - schema has no denormalized close_time column; close time only available inside XDR blob

### Findings

The inefficiency is confirmed and is in the hot path:

1. **The inefficiency exists.** `getLedgerRangeWithCache` fetches the full `meta` BLOB from SQLite and fully deserializes it via `db.Select(ctx, &lcm, query)` into `xdr.LedgerCloseMeta`. The LCM blob can be large (KB to MB depending on ledger transaction count). All that data is allocated, deserialized, and immediately discarded - only two scalar fields are read.

2. **It is in a hot path.** Every `getTransactions` request executes this path unconditionally (line 217). For tip-polling workloads (small limit, frequent requests), this adds a fixed per-request overhead.

3. **The proposed fix is correct.** The oldest ledger's sequence and close time change only when `trimLedgers` runs during `writeTx.Commit`. Between commits (~5s cadence), the oldest ledger is stable. Extending `dbCache` with `oldestLedgerSeq` and `oldestLedgerCloseTime` fields and updating them at commit time (or invalidating and letting the first subsequent read repopulate) is safe under the existing `sync.RWMutex` synchronization model.

4. **Impact estimate.** At 100 RPS with 5s ledger cadence, ~500 requests per commit cycle each perform a full LCM decode for range info. Caching reduces this to at most 1 decode per commit cycle - a ~500x reduction in range-query deserialization work. However, relative to total request cost (which includes batch LCM reads for actual transaction data), the improvement is small: Low severity is appropriate. For tip-polling with `limit=1`, the range-query LCM decode can represent up to ~50% of total XDR deserialization work (1 wasted decode vs 1 useful decode from the 50-ledger batch), making the savings more noticeable in that scenario.

5. **No existing optimizations cover this.** There is no object pool, no caching, and no partial decode for the oldest-ledger query path. The `BatchGetLedgers` method (ledger.go:74-116) demonstrates a partial-decode pattern (extracting just the header) that could also be applied if a full cache approach is not used.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/db/db.go` (extend `dbCache` struct and `Commit` method) and `cmd/stellar-rpc/internal/db/ledger.go` (update `getLedgerRangeWithCache`, `ledgerReader.GetLedgerRange`, `ledgerReader.NewTx`, and `ledgerReaderTx.GetLedgerRange`)
- **Change description**: Add `oldestLedgerSeq uint32` and `oldestLedgerCloseTime int64` to `dbCache`. In `getLedgerRangeWithCache`, check if oldest is cached (nonzero); if so, return both ends from cache without any DB query. In `commitAndUpdateCache`, after trimming, either (a) compute the new oldest seq from `latestSeq + 1 - retentionWindow` and read its close time via a partial-decode query, or (b) simply invalidate by setting oldest fields to 0 so the next read repopulates. Option (b) is simpler and still eliminates ~99.8% of redundant decodes. Update `ledgerReaderTx` to also snapshot `oldestLedgerSeq`/`oldestLedgerCloseTime` from cache, and update `ResetCache()` to zero the new fields.
- **Correctness check**: `BenchmarkGetLedgerRange` (ledger_test.go:187-197) and existing `TestGetLedgerRange*` tests cover this code path. Ensure cached values match DB values by running range queries before and after commits.
- **Benchmark focus**: Run `BenchmarkGetLedgerRange` before and after. Expect significant reduction in allocations per operation (ns/op may improve 5-50x for the range query itself). For end-to-end getTransactions latency with `limit=1` tip-polling, expect <5% improvement (Low severity) since batch LCM reads dominate.

---

## PoC Attempt

**Result**: POC_PASS
**Date**: 2026-04-07
**PoC by**: claude-opus-4-6, high

### Changes Made

1. **`cmd/stellar-rpc/internal/db/db.go`** (lines 54-55): Added `oldestLedgerSeq uint32` and `oldestLedgerCloseTime int64` fields to the `dbCache` struct, extending the write-through cache to track both ends of the retention window.

2. **`cmd/stellar-rpc/internal/db/db.go`** (lines 69-70): Updated `ResetCache()` to zero the new oldest-ledger fields alongside the existing latest-ledger fields.

3. **`cmd/stellar-rpc/internal/db/db.go`** (lines 173-176): Updated `getLatestLedgerSequence()` to backfill the oldest cache from ledger range data when it's missing (cold-start path).

4. **`cmd/stellar-rpc/internal/db/db.go`** (lines 351-357): Added oldest-cache invalidation in `commitAndUpdateCache()`. When trimming advances the retention window past the cached oldest ledger, the oldest fields are zeroed so the next read repopulates them (option (b) from the reviewer's guidance).

5. **`cmd/stellar-rpc/internal/db/ledger.go`** (lines 63-64): Added `oldestLedgerSeq` and `oldestLedgerCloseTime` fields to `ledgerReaderTx` so read transactions snapshot both ends of the cache.

6. **`cmd/stellar-rpc/internal/db/ledger.go`** (lines 69-80): Added full-cache fast path to `ledgerReaderTx.GetLedgerRange()` - when both bounds are cached, returns immediately without any DB query.

7. **`cmd/stellar-rpc/internal/db/ledger.go`** (lines 230-231): Updated `ledgerReader.NewTx()` to snapshot oldest-ledger cache fields into the `ledgerReaderTx` under the existing `RLock`.

8. **`cmd/stellar-rpc/internal/db/ledger.go`** (lines 292-340): Rewrote `ledgerReader.GetLedgerRange()` with three tiers: (a) both cached -> instant return, (b) latest cached -> query only oldest and backfill cache, (c) neither cached -> query both and backfill cache.

### Demonstration

The optimization eliminates the unconditional full XDR deserialization of the oldest retained `LedgerCloseMeta` blob on every `getTransactions` request. By caching both the oldest and latest ledger sequence/close-time in `dbCache`, the hot `GetLedgerRange()` path returns two cached scalars with zero DB I/O and zero XDR deserialization. The cache is invalidated (zeroed) only when `trimLedgers` advances the retention window during commit, so at most one request per ~5s commit cycle repopulates the oldest cache via the existing `getLedgerRangeWithCache` fallback - a ~500x reduction in range-query deserialization work at 100 RPS.

### Test Results

All 12 Go test packages in `cmd/stellar-rpc/internal/...` pass with `-race` flag, including `db` (2.88s), `methods` (1.59s), `feewindow` (6.40s), `ingest` (1.07s), and `integrationtest` (1.50s). Rust tests also pass (1 test in xdr2json crate). No test failures or regressions.

---

## Final Review - Needs Revision

**Date**: 2026-04-07
**Final review by**: gpt-5.4, high

### What Needs Fixing

The code-level inefficiency is real, and the isolated DB benchmark improved dramatically, but the required `stellar-rpc-blaster` benchmark did **not** show a stable end-to-end `getTransactions` win. In the main sweep, both builds had the same 100 RPS throughput ceiling under the 20% p50 step-up rule. Initial 100 RPS numbers were only slightly better for the optimized build (`17.823 ms -> 17.711 ms` p50), but the focused 100 RPS rerun reversed direction (`17.487 ms -> 18.031 ms` p50), which makes the endpoint-level effect indistinguishable from run-to-run noise.

### Revision Instructions

Either:

1. Reframe this finding to **Informational** and explicitly state that it is a real internal micro-optimization with no reproducible end-to-end `getTransactions` improvement under the project's benchmarking methodology, or
2. Provide stronger benchmark evidence using `stellar-rpc-blaster` that shows a consistent win across controlled reruns at the same RPS level, not just a single near-noise delta.

If you keep the finding in performance scope, use the project's benchmark results as the authoritative evidence, not the isolated `BenchmarkGetLedgerRange` result.

### Checks Passed So Far

- The waste exists in the `getTransactions -> GetLedgerRange()` path and the optimization removes it.
- The isolated optimized tree built successfully with `make -j8 build-stellar-rpc` and passed `make go-test`.
- The targeted DB benchmark improved substantially: `BenchmarkGetLedgerRange` went from `32348 ns/op, 6807 B/op, 94 allocs/op` to `1662 ns/op, 16 B/op, 4 allocs/op`.
- Safety review did not find snapshot/cache consistency bugs in the read-transaction or commit invalidation logic.
