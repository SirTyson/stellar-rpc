# H001: Commit-time cache locking serializes `getTransactions` read starts at every ledger close

**Date**: 2026-04-07
**Subsystem**: ingest
**Severity**: Medium
**Impact**: lock contention / read-transaction startup latency
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

`getTransactions` should be able to start its SQLite read transaction and snapshot cached ledger bounds without waiting on unrelated ingest-side bookkeeping. At ledger-close time, concurrent pollers should pay only the actual DB/read cost for their request, not block behind a coarse in-process mutex while the writer commits.

## Mechanism

`writeTx.Commit()` holds `globalCache.Lock()` across the underlying SQLite `w.tx.Commit()`, while `ledgerReader.NewTx()` holds `cache.RLock()` across `Clone()` and `BeginTx()`. Because every `getTransactions` request begins with `ledgerReader.NewTx()`, requests arriving during tip-following ingest commit are forced to wait for the writer lock even before they can open their read snapshot. Narrowing the critical section to just the cache-field copy/update, or versioning the cache separately from the DB commit path, would remove a periodic read-start serialization point that is synchronized with the exact moment clients poll the newest ledger.

## Trigger

1. Run normal tip-following ingest so a write commit happens on every new ledger.
2. Send many concurrent `getTransactions` requests for `startLedger=latest` immediately after each close.
3. Compare current p95/p99 latency against a version where `NewTx()` snapshots cache fields without holding the RWMutex through `BeginTx()`, and `Commit()` updates cache fields outside the expensive commit body.

## Target Code

- `cmd/stellar-rpc/internal/db/db.go:337-358` — `writeTx.Commit()` holds `w.globalCache.Lock()` while calling `w.tx.Commit()`.
- `cmd/stellar-rpc/internal/db/ledger.go:170-184` — `ledgerReader.NewTx()` holds `cache.RLock()` across `Clone()` and `BeginTx()`.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:315-326` — every request starts with `ledgerReader.NewTx()` and `GetLedgerRange()`.
- `cmd/stellar-rpc/internal/ingest/service.go:198-224` — tip-following ingest commits once per ledger, creating a periodic contention point.

## Evidence

The lock scope is broader than the cache work itself on both sides. Readers need only four scalar fields from the cache, but they keep the read lock while opening the SQLite transaction; writers need atomic cache publication, but they keep the write lock while the DB commit runs. That makes the cache mutex a shared choke point for every new `getTransactions` request exactly when ingest is committing the freshest ledger.

## Anti-Evidence

Commit happens only once per ledger and may be short on a warm local SQLite database, so the benefit is workload-dependent. If `w.tx.Commit()` is typically sub-millisecond, the improvement will be concentrated in tail latency under synchronized poller bursts rather than average latency.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

The contention mechanism is confirmed. `commitAndUpdateCache()` (db.go:340-358) holds an exclusive `sync.RWMutex` Lock across the SQLite transaction commit (`w.tx.Commit()` → `Session.Commit()` → `s.tx.Commit()`), which writes all buffered inserts/deletes to the WAL file. Concurrently, every `getTransactions` request calls `ledgerReader.NewTx()` (ledger.go:170-184), which holds RLock across `Clone()` (trivial—copies a DB pointer) and `BeginTx()` (SQLite read-transaction open). Go's `sync.RWMutex` blocks new RLock callers once a Lock is pending, so the writer's commit creates a brief thundering-herd stall for all readers arriving at ledger-close time. The contention is bidirectional: the writer must also wait for any in-flight readers to release their RLock before it can acquire Lock.

### Code Paths Examined

- `cmd/stellar-rpc/internal/db/db.go:340-358` — `commitAndUpdateCache` takes exclusive Lock, calls `w.tx.Commit()` (SQLite WAL write), updates 4 scalar cache fields, defers Unlock. The WAL checkpoint (`PRAGMA wal_checkpoint(TRUNCATE)`) runs in `postCommit` AFTER the lock is released (line 368), so it does not contribute to contention.
- `cmd/stellar-rpc/internal/db/ledger.go:170-184` — `NewTx` takes RLock, calls `r.db.Clone()` (copies `*sql.DB` pointer, ~nanoseconds), calls `BeginTx` (opens SQLite read snapshot, ~10-50µs), reads 4 cache scalars, defers RUnlock.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:315` — `fetchLedgerMetas` calls `h.ledgerReader.NewTx(ctx)` as its first operation, confirming every request hits the lock.
- `cmd/stellar-rpc/internal/ingest/service.go:222` — `tx.Commit(ledgerCloseMeta, durationMetrics)` is called once per ingested ledger (~every 5 seconds on mainnet).
- `go-stellar-sdk/support/db/session.go:63-86,101-104` — `BeginTx` calls `s.DB.BeginTxx(ctx, opts)` (database/sql round-trip); `Clone` returns a new Session sharing the same `*sql.DB`.
- `cmd/stellar-rpc/internal/db/db.go:73-79` — SQLite is opened with `_journal_mode=WAL&_wal_autocheckpoint=0&_synchronous=NORMAL`, meaning commits avoid fsync (only WAL append), keeping commit time low but not zero.

### Findings

The inefficiency is real but its magnitude is bounded by three factors:

1. **Low writer frequency**: The commit happens once per ~5-second ledger close. Only requests overlapping with that brief window experience contention.

2. **Fast commit in WAL+NORMAL mode**: Without fsync, the SQLite commit is a WAL-file append of dirty pages. For a typical ledger (tens of KB of inserts + trims), this is likely sub-millisecond to low single-digit milliseconds on warm storage.

3. **Bidirectional stall is brief**: The writer waits for at most a handful of in-flight readers (each holding RLock for ~50µs during BeginTx), then holds Lock for the commit duration. Total reader blockage ≈ writer commit time.

Under synchronized poller bursts (many clients polling `startLedger=latest` right after each close), the contention is real and measurable in p95/p99 tail latency. However, average latency and throughput impact is small because the contention event is infrequent (~0.2 Hz) and short-lived (~1ms). This places the impact below the Medium threshold (5-20%) and into Low (<5% but measurable, concentrated in tail latency under burst workloads).

The reader-side fix is straightforward and safe: snapshot the 4 cache scalars under RLock first (nanoseconds), release RLock, then do Clone()+BeginTx() without holding any lock. Cache values may be momentarily stale relative to the read snapshot (reader reports `latest=N` while its snapshot contains `N+1`), but this is conservative—the reader never serves data it claims doesn't exist, and the next request sees the updated cache.

The writer-side fix (commit without lock, update cache under lock) is also sound given the single-writer design and the monotonic guard (`ledgerSeq > w.globalCache.latestLedgerSeq`), but changes the intentional atomicity documented in the code comment at line 337-339.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/db/ledger.go:170-184` (reader-side fix only — lowest risk, highest clarity)
- **Change description**: In `ledgerReader.NewTx()`, read the 4 cache fields under RLock into locals, release RLock, then call `Clone()` and `BeginTx()` without holding the lock. Construct `ledgerReaderTx` from the locals. This eliminates reader-side lock holding through the SQLite BeginTx call.
- **Correctness check**: Existing tests in `cmd/stellar-rpc/internal/db/ledger_test.go` and integration tests in `cmd/stellar-rpc/internal/methods/get_transactions_test.go` should continue to pass. The change is safe because stale cache values only make the reader report a slightly older ledger range, never an inconsistent one.
- **Benchmark focus**: Measure p95/p99 latency of `getTransactions` under concurrent load with a synthetic ingest writer committing every 5 seconds. The improvement should appear as reduced tail latency spikes coinciding with commit events. Expect 1-5ms p99 reduction under high concurrency (>500 RPS) during ledger close bursts.
