# H004: Every getTransactions Request Still Pays an Oldest-Ledger Query Because the Cache Tracks Only the Latest Bound

**Date**: 2026-04-06
**Subsystem**: methods
**Severity**: Low
**Impact**: latency / DB I/O
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

For the common case where `getTransactions` returns a small page near the latest ledger, range metadata should come from cache without an extra SQL read. A request that only needs to confirm the newest ledger bounds should not fetch and deserialize the oldest ledger on every call.

## Mechanism

`getTransactionsByLedgerSequence` always begins by calling `readTx.GetLedgerRange(ctx)`. Even when the latest ledger sequence and close time are already cached, `getLedgerRangeWithCache` still issues a DB query for the first ledger because the cache only stores `latestLedgerSeq` and `latestLedgerCloseTime`. That leaves a fixed per-request lookup and `LedgerCloseMeta` deserialize on the hot path, which should be noticeable for small, latest-page requests where the rest of the work is intentionally small.

## Trigger

1. Send a high volume of `getTransactions` requests with small limits against the newest ledgers.
2. Keep the page size small enough that most requests are satisfied by one or two ledgers.
3. Compare latency and query counts against a version that also caches the oldest ledger sequence/close time and only refreshes that cache when retention trimming advances the window.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:221-240` — every request calls `GetLedgerRange` before doing any pagination work.
- `cmd/stellar-rpc/internal/db/ledger.go:ledgerReaderTx.GetLedgerRange:60-64` — the transactional reader always defers to `getLedgerRangeWithCache` when latest bounds are cached.
- `cmd/stellar-rpc/internal/db/ledger.go:getLedgerRangeWithCache:223-250` — the helper still queries `MIN(sequence)` and deserializes the oldest ledger's metadata.
- `cmd/stellar-rpc/internal/db/db.go:dbCache:49-54` — the cache stores only latest ledger fields, so the oldest bound is never memoized.

## Evidence

The DB cache type has no oldest-ledger fields, and `getLedgerRangeWithCache` explicitly says it "only needs to look up the first ledger since we have the latest cached." `getTransactions` unconditionally calls that helper even for requests that immediately paginate near the latest ledger, so the extra query is guaranteed on every request.

## Anti-Evidence

This is a fixed-cost optimization, so it matters most when a request is otherwise cheap. Large historical scans or event-heavy JSON pages will still be dominated by ledger walking and serialization, and any oldest-ledger cache has to stay coherent with retention trimming as the window advances.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full call chain from `getTransactionsByLedgerSequence` (line 232) through `ledgerReaderTx.GetLedgerRange` (line 60-64) into `getLedgerRangeWithCache` (lines 223-250). Confirmed that when the cache is warm (`latestLedgerSeq != 0`), the function still executes `SELECT meta FROM ledger_close_meta WHERE sequence = (SELECT MIN(sequence) FROM ledger_close_meta)`, reading the full `meta` BLOB and deserializing the entire `xdr.LedgerCloseMeta` just to extract `LedgerSequence()` and `LedgerCloseTime()`. The `sequence` column is `INTEGER PRIMARY KEY` (01_init.sql:15), so the `MIN()` subquery is O(1) via the B-tree, but the full BLOB read and XDR deserialization of a potentially large ledger (all transactions, results, fee changes) is the expensive part. The `dbCache` struct (db.go:49-54) stores only `latestLedgerSeq` and `latestLedgerCloseTime`, confirming no oldest-ledger caching exists.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:232` — `readTx.GetLedgerRange(ctx)` called unconditionally on every request
- `cmd/stellar-rpc/internal/db/ledger.go:60-64` — `ledgerReaderTx.GetLedgerRange` delegates to `getLedgerRangeWithCache` when cache is warm
- `cmd/stellar-rpc/internal/db/ledger.go:223-250` — `getLedgerRangeWithCache` executes SQL query for MIN(sequence), reads full `meta` BLOB, deserializes entire `xdr.LedgerCloseMeta`, extracts only `.LedgerSequence()` and `.LedgerCloseTime()`
- `cmd/stellar-rpc/internal/db/db.go:49-54` — `dbCache` stores only latest ledger fields; no oldest-ledger fields
- `cmd/stellar-rpc/internal/db/db.go:301-353` — `writeTx.Commit` updates cache atomically with commit; `trimLedgers` (line 306) advances the oldest bound but doesn't cache it
- `cmd/stellar-rpc/internal/db/ledger.go:130-143` — `ledgerReader.NewTx` snapshots cache values into `ledgerReaderTx` at transaction start; same pattern would work for oldest fields
- `cmd/stellar-rpc/internal/db/sqlmigrations/01_init.sql:14-17` — `sequence INTEGER NOT NULL PRIMARY KEY` confirms O(1) MIN lookup, but BLOB read + deserialization remains costly
- `cmd/stellar-rpc/internal/methods/get_transactions.go:279-286` — response uses `FirstLedger.Sequence` and `FirstLedger.CloseTime`, confirming the data is needed

### Findings

1. **The inefficiency is real**: Every `getTransactions` request deserializes the full oldest-ledger `LedgerCloseMeta` BLOB (which can be tens to hundreds of KB depending on transaction volume) just to extract two integer fields. This is pure waste when the oldest ledger changes only once per retention trim cycle (~every 5 seconds on mainnet).

2. **It's in the hot path**: `GetLedgerRange` is called on every `getTransactions` request (line 232), before any pagination or data fetch work begins.

3. **The fix is correct and safe**: Adding `oldestLedgerSeq` and `oldestLedgerCloseTime` to `dbCache` mirrors the existing `latestLedger*` pattern. The cache would be populated on first access and updated atomically in `writeTx.Commit` after `trimLedgers` runs. The same lock (`dbCache.RWMutex`) already guards the latest fields, so adding oldest fields to the same critical section preserves consistency. The `NewTx` method (line 130-143) already snapshots cache values into `ledgerReaderTx`, so the same pattern extends naturally.

4. **Impact estimate**: For a small-page request (limit=1, near latest ledger), the per-request cost is approximately: 1 `GetLedgerRange` SQL+deser + 1 `fetchLedgerData` SQL+deser + transaction parsing. Eliminating the `GetLedgerRange` SQL+deser saves roughly one of two SQL+deserialization operations, which could be ~30-50% of DB I/O cost for minimal requests. However, total end-to-end latency includes JSON-RPC framing, network, and serialization overhead, so the realistic improvement is <5% overall — consistent with Low severity.

5. **No existing optimizations cover this**: There is no pool, cache, or batch that mitigates this query. The `dbCache` explicitly omits the oldest bound.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/db/db.go` (add `oldestLedgerSeq` and `oldestLedgerCloseTime` to `dbCache`), `cmd/stellar-rpc/internal/db/ledger.go` (update `getLedgerRangeWithCache` to skip SQL when both bounds are cached; update `NewTx` to snapshot oldest fields; update `ledgerReaderTx.GetLedgerRange` to use fully-cached path), `cmd/stellar-rpc/internal/db/db.go:writeTx.Commit` (update oldest cache after `trimLedgers` or on first population)
- **Change description**: Extend `dbCache` with `oldestLedgerSeq`/`oldestLedgerCloseTime`. In `getLedgerRangeWithCache`, return immediately when both oldest and latest are cached. Populate oldest cache on first `GetLedgerRange` call. Update oldest cache in `Commit` after `trimLedgers` advances the window (set to `latestLedgerSeq + 1 - retentionWindow` when trimming occurs). Add `ResetCache` coverage for new fields.
- **Correctness check**: Existing tests for `GetLedgerRange` in `cmd/stellar-rpc/internal/db/ledger_test.go` should continue to pass. Also verify `getTransactions` integration tests still return correct `OldestLedger`/`OldestLedgerCloseTime` values.
- **Benchmark focus**: Measure `getTransactions` latency at limit=1 with startLedger near latest. The oldest-ledger SQL query count per request should drop from 1 to 0 (after first request warms the cache). Expect 1-5% latency reduction for small-page queries; larger pages will see proportionally smaller improvement.
