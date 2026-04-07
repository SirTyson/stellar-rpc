# H001: ledgerReader.NewTx Cache Lock Blocks getTransactions Throughput

**Date**: 2026-04-07
**Subsystem**: daemon
**Severity**: Low
**Impact**: lock contention
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Starting a read transaction for `getTransactions` should not hold shared locks long enough to materially interfere with ingestion or other readers. Any cache synchronization around `NewTx` should be limited to copying the cached ledger tip metadata, not to the full request lifetime.

## Mechanism

I suspected `ledgerReader.NewTx` might serialize `getTransactions` against ingest because it takes `db.cache.RLock()` before cloning the session and beginning the SQLite read transaction. If that lock were held across the hot path, concurrent readers could block the writer's cache update and turn request setup into a measurable contention point.

## Trigger

Hammer `getTransactions` concurrently while ingestion is active and inspect blocking profiles around `db.cache.RLock()` / `db.cache.Lock()`.

## Target Code

- `cmd/stellar-rpc/internal/db/ledger.go:NewTx:155-167` — acquires `db.cache.RLock()` before `Clone()` and `BeginTx()`.

## Evidence

The read lock is indeed taken before the SQLite transaction begins, so there is at least a theoretical window where reader startup and writer cache updates touch the same mutex.

## Anti-Evidence

The lock is released immediately after `BeginTx()` and the two cached fields are copied into the transaction wrapper; none of the expensive ledger scanning, batching, or transaction decoding happens while the lock is held.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The shared cache lock only covers read-transaction setup, not the `getTransactions` hot path. Any contention here is short-lived and front-loaded, so it is unlikely to explain a meaningful latency or throughput delta for the endpoint.

### Lesson Learned

For daemon-layer `getTransactions` work, focus on wrappers and allocations that persist for the full request or scale with response size. Tiny setup locks that are released before the batch ledger scan are poor optimization targets.
