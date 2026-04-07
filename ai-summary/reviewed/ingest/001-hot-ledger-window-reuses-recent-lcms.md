# H001: Tip-following ingest never reuses freshly ingested `LedgerCloseMeta` blobs for hot `getTransactions` pages

**Date**: 2026-04-07
**Subsystem**: ingest
**Severity**: Medium
**Impact**: DB I/O / XDR decode / allocation overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When `getTransactions` is repeatedly called against the newest retained ledgers, the request path should be able to reuse the exact `LedgerCloseMeta` blobs that tip-following ingest just fetched and committed. Small tip-polling pages should not have to reopen SQLite, reread the same `meta` BLOBs, and fully unmarshal them again when those ledgers are already resident in memory on the ingest side.

## Mechanism

`ingest.Service.ingest()` already fetches each new `xdr.LedgerCloseMeta` into memory, but after commit it retains only `latestIngestedSeq`; the actual ledger blob is discarded. `getTransactionsByLedgerSequence()` then always opens a read transaction and calls `BatchGetLedgerMetas()`, which scans the same `meta` BLOBs back out of SQLite and fully unmarshals them into `[]xdr.LedgerCloseMeta`. A bounded recent-ledger window populated by ingest (for example via the existing `ledgerbucketwindow` primitive) could let hot recent pages bypass both the SQLite read and the `Scan`/`UnmarshalBinary` cost for the newest ledgers.

## Trigger

1. Run continuous tip-following ingestion.
2. Send many `getTransactions` requests with `startLedger` at or near the latest retained ledger and a small `limit` (for example 1-10).
3. Compare the current code against a version that serves the newest few ledgers from an ingest-populated in-memory window before falling back to SQLite.

## Target Code

- `cmd/stellar-rpc/internal/ingest/service.go:190-242` — tip-following ingest fetches `ledgerCloseMeta`, commits it, and only retains `latestIngestedSeq`.
- `cmd/stellar-rpc/internal/ingest/service.go:245-292` — range ingest updates only sequence metadata after backfill/frontfill commits.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:206-310` — every request opens a DB read transaction and batch-fetches ledger metas from SQLite.
- `cmd/stellar-rpc/internal/db/ledger.go:120-139` — `BatchGetLedgerMetas()` eagerly scans full `xdr.LedgerCloseMeta` values from the `meta` BLOB column.
- `cmd/stellar-rpc/internal/ledgerbucketwindow/ledgerbucketwindow.go:9-120` — existing bounded contiguous-ledger window abstraction that could host a recent-ledger cache.
- `cmd/stellar-rpc/internal/feewindow/feewindow.go:37-73` — prior art for keeping bounded per-ledger state alive across requests.

## Evidence

The ingest service already sees every new ledger exactly once on the hot path (`GetLedger` in `service.go`) but does not publish any reusable recent-ledger artifact beyond the latest sequence gauge. The request path for `getTransactions` is therefore forced back through `ledgerReader.NewTx()` and `BatchGetLedgerMetas()`, even for the freshest ledgers that are likely to be queried repeatedly by pollers. The repository already has a production pattern for contiguous in-memory ledger windows (`ledgerbucketwindow` via `feewindow`), so the missing piece is reuse, not a lack of windowing infrastructure.

## Anti-Evidence

This only helps requests that hit a small recent-ledger hot set; wide historical scans still need the DB path. Any implementation has to reset or repopulate the window correctly after backfill, restart, or retention-window eviction so that cached ledgers never diverge from what the DB would return.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced from `Service.ingest()` (service.go:190-242) through to `getTransactionsByLedgerSequence` (get_transactions.go:203-310). Confirmed that `ingest()` fetches `LedgerCloseMeta` via `s.ledgerBackend.GetLedger(ctx, sequence)` at line 192, passes it through `ingestLedgerCloseMeta` and `feeWindows.IngestFees`, then discards it after commit — only `latestIngestedSeq` (a uint32) survives. Every subsequent `getTransactions` request re-opens a SQLite read transaction, calls `BatchGetLedgerMetas` which issues a SQL SELECT on the `meta` BLOB column, and fully deserializes each blob via `xdr.LedgerCloseMeta`'s `Scan` method. The `dbCache` struct (db.go:49-54) only caches `latestLedgerSeq` and `latestLedgerCloseTime` — there is no in-memory cache of LCM blobs anywhere in the codebase.

### Code Paths Examined

- `cmd/stellar-rpc/internal/ingest/service.go:190-242` — `ingest()` fetches LCM at line 192, writes to DB via `ingestLedgerCloseMeta`, commits at line 222, only retains `latestIngestedSeq` at lines 238-241. The `ledgerCloseMeta` local variable goes out of scope.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:203-310` — `getTransactionsByLedgerSequence` opens read tx at line 206, fetches LCMs in batches of 50 via `BatchGetLedgerMetas` at line 263, then processes each via `processTransactionsInLedger` at line 290.
- `cmd/stellar-rpc/internal/db/ledger.go:120-139` — `BatchGetLedgerMetas` issues `SELECT meta FROM ledger_close_meta WHERE sequence >= ? AND sequence <= ? ORDER BY sequence ASC` and deserializes results into `[]xdr.LedgerCloseMeta` via `l.tx.Select`.
- `cmd/stellar-rpc/internal/db/db.go:49-67` — `dbCache` only stores `latestLedgerSeq` and `latestLedgerCloseTime`; no LCM blob cache exists.
- `cmd/stellar-rpc/internal/ledgerbucketwindow/ledgerbucketwindow.go:10-120` — Generic `LedgerBucketWindow[T]` circular buffer with `Append`, `Get`, contiguity enforcement, and eviction. Already used by `FeeWindow` with `T = []uint64`.
- `cmd/stellar-rpc/internal/feewindow/feewindow.go:37-65` — `FeeWindow` uses `LedgerBucketWindow[[]uint64]` with `sync.RWMutex` for concurrent access. Provides the pattern for a similar LCM cache.

### Findings

The inefficiency is real and architecturally confirmed:

1. **No LCM reuse exists**: The ingest goroutine and the request goroutines share no in-memory LCM state. Every `getTransactions` call that touches recent ledgers pays the full SQLite query + XDR deserialization cost, even for the same ledger that ingest committed moments ago.

2. **XDR deserialization is the main waste**: In WAL mode with the OS page cache warm, the SQLite I/O cost for recently-written BLOBs is low. The dominant redundant cost is the `xdr.LedgerCloseMeta` deserialization (via the `Scan` interface in `l.tx.Select`), which must reconstruct the entire object tree — ledger headers, transaction sets, results, metas — from the binary blob on every call.

3. **Existing pattern is ready to reuse**: `LedgerBucketWindow[xdr.LedgerCloseMeta]` could hold the last N ledgers. `FeeWindow` already demonstrates the mutex-guarded concurrent access pattern. The ingest goroutine would `Append` each new LCM, and request handlers would check the window before falling back to `BatchGetLedgerMetas`.

4. **Severity downgrade to Low**: While the optimization is correct, `getTransactions`'s dominant cost is per-transaction processing in `processTransactionsInLedger` — creating `LedgerTransactionReader`, iterating transactions, calling `ParseTransaction`, and encoding results (base64/JSON). The LCM retrieval + unmarshal is the first step but not the bottleneck. For single-ledger tip-polling with few transactions, the saving could approach 5-10%. For multi-ledger or transaction-heavy requests, it's a smaller fraction. Additionally, `processTransactionsInLedger` receives `xdr.LedgerCloseMeta` by value (get_transactions.go:77), so even with a cache the LCM is copied into the function — though this is a Go value copy from already-deserialized memory, which is cheaper than XDR deserialization.

5. **Memory consideration**: A `LedgerCloseMeta` for a busy mainnet ledger can be several hundred KB to a few MB. A window of 50 ledgers (matching the batch size) could consume 50-250 MB. A smaller window (e.g., 10 ledgers) would be more practical and still serve most tip-polling workloads.

### PoC Guidance

- **Target code**: Add a `LedgerBucketWindow[xdr.LedgerCloseMeta]` field (guarded by `sync.RWMutex`) to `ingest.Service` or to a shared struct accessible by both the ingest goroutine and request handlers. Populate it in `Service.ingest()` after line 222 (post-commit). On the read side, add a method to `LedgerReader` (or a new cache-aware wrapper) that checks the window before falling back to `BatchGetLedgerMetas`.
- **Change description**: In `service.go`, after `tx.Commit()`, append the `ledgerCloseMeta` to the in-memory window. In `get_transactions.go`, before calling `readTx.BatchGetLedgerMetas()`, check if the requested range falls within the cached window and serve from there if so. If only part of the range is cached, serve the cached portion and fall back to SQLite for the rest.
- **Correctness check**: Existing tests for `getTransactions` in `cmd/stellar-rpc/internal/methods/get_transactions_test.go` cover the DB-backed path. The cache should be transparent — same results either way. `daemon.go:220-221` already calls `ResetCache()` after backfill; the LCM window should be reset there too. The `LedgerBucketWindow.Append` method already enforces contiguity (line 37), so stale or out-of-order entries cannot corrupt the cache.
- **Benchmark focus**: Measure `getTransactions` p50/p99 latency for `startLedger = latest, limit = 1-5` under 50+ concurrent pollers. The LCM retrieval + unmarshal portion should drop to near-zero for cached hits. Expect <5% end-to-end latency improvement for typical workloads, potentially higher for high-concurrency single-ledger polling.

---

## PoC Attempt

**Result**: POC_PASS
**Date**: 2026-04-07
**PoC by**: claude-opus-4.6, high

### Changes Made

1. **`cmd/stellar-rpc/internal/ingest/lcm_cache.go`** (new file) — `LCMCache` struct wrapping `LedgerBucketWindow[xdr.LedgerCloseMeta]` with `sync.RWMutex`. Provides `Append`, `GetAllLCMs`, and `Reset` methods. Default window size is 10 ledgers. `GetAllLCMs` returns all requested LCMs only on a full cache hit (all sequences present); returns false on any miss, forcing the caller to fall back to SQLite.

2. **`cmd/stellar-rpc/internal/ingest/service.go`** (lines 32, 131, 80, 224-232) — Added `LCMCache *LCMCache` field to `Config` and `lcmCache *LCMCache` field to `Service`. Wired through `newService`. After `tx.Commit()` in the tip-following `ingest()` method, appends the freshly ingested `ledgerCloseMeta` to the cache. Cache is not populated during `ingestRange` (backfill).

3. **`cmd/stellar-rpc/internal/methods/get_transactions.go`** (lines 21, 27, 409-431, 523-540) — Added `lcmCache *rpcIngest.LCMCache` field to `transactionsRPCHandler`. In `getTransactionsByLedgerSequence`, before the SQLite fetch, checks whether all requested ledger sequences are in the cache (non-JSON format only, since JSON needs raw bytes for the Rust FFI). On full cache hit, releases the SQLite read snapshot immediately and serves deserialized LCMs directly. On miss, falls through to the existing SQLite path. Updated `NewGetTransactionsHandler` to accept and wire the cache.

4. **`cmd/stellar-rpc/internal/jsonrpc.go`** (lines 28, 70) — Added `LCMCache *ingest.LCMCache` to `HandlerParams` and passed it to `NewGetTransactionsHandler`.

5. **`cmd/stellar-rpc/internal/daemon/daemon.go`** (lines 195, 208, 225, 356, 369) — Created `LCMCache` before the ingest service, passed it to `ingest.Config` and `createIngestService`. Added `lcmCache.Reset()` after backfill alongside `db.ResetCache()` and `feewindows.Reset()`. Passed the cache to `createJSONRPCHandler` and into `HandlerParams`.

### Demonstration

The optimization adds a 10-ledger in-memory LCM cache populated by the tip-following ingest goroutine after each successful commit. For `getTransactions` requests in the default XDR/base64 format that target the most recent ledgers, the handler serves deserialized `LedgerCloseMeta` directly from the cache, completely bypassing the SQLite read transaction and the `UnmarshalBinary` deserialization. This eliminates the two redundant steps (SQL query + XDR decode) that every tip-polling request previously paid, even when the ingest goroutine had just committed the same data moments earlier.

### Test Results

All Go tests pass: `go test ./...` reports ok for all 12 testable packages, including `methods` (10 getTransactions tests covering default/custom limits, cursors, JSON format, missing ledgers, and error cases) and `ingest` (ingestion service tests). Build succeeds with `make build-stellar-rpc`.

---

## Final Review — Needs Revision

**Date**: 2026-04-07
**Final review by**: gpt-5.4, high

### What Needs Fixing

The implementation trace is real, but the performance proof is not yet good enough to confirm. The official `stellar-rpc-blaster` workload does not line up cleanly with the optimization: it randomizes `getTransactions` request bodies and uses JSON roughly 50% of the time, while this cache only affects full-cache-hit, non-JSON requests in the newest 10-ledger window. On top of that, my independent benchmark attempt on isolated baseline/optimized worktrees could not produce a valid before/after comparison because the local review server became unstable under the required benchmark setup on this shared host, and the only partial baseline run I captured already showed heavy errors at 50 RPS (46 ok / 1078 err by 25s), which is not usable evidence for a measured improvement.

### Revision Instructions

1. Re-run the official `stellar-rpc-blaster` baseline vs optimized sweep in an environment where the review server can stay up for the full benchmark window without captive-core/live-network port conflicts.
2. Produce independent before/after blaster outputs with zero-error runs and extract the authoritative p50/p95/p99 and ceiling numbers from those runs.
3. Either:
   - broaden the optimization so it materially affects the official mixed-format workload, or
   - reframe/downgrade the claim to hot base64 tip-polling only (likely Informational unless the official benchmark shows a measurable gain).

### Checks Passed So Far

- Code trace passed: tip-following ingest appends post-commit `LedgerCloseMeta` values into an in-memory window, and `getTransactions` consults that window before the DB path on non-JSON full-cache hits.
- Isolation/build validation passed: independent baseline and optimized worktrees both built cleanly and passed `make go-test`.
- Scope check passed: the change stays in the `getTransactions` call chain and does not require DB schema or third-party dependency changes.
