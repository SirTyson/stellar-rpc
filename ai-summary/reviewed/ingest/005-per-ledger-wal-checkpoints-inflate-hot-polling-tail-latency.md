# H005: Per-ledger `wal_checkpoint(TRUNCATE)` inflates `getTransactions` tail latency during hot polling

**Date**: 2026-04-07
**Subsystem**: ingest
**Severity**: Medium
**Impact**: SQLite I/O contention / tail latency spikes
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

`getTransactions` pollers hitting the newest ledger should not be forced to overlap with a full WAL checkpoint on every ingest commit. The endpoint should benefit from WAL's read/write decoupling instead of repeatedly paying for checkpoint-driven file truncation and sync work at the exact moment new-ledger traffic arrives.

## Mechanism

After every ingest commit, `writeTx` runs `PRAGMA wal_checkpoint(TRUNCATE)` synchronously. Tip-following ingest commits once per ledger, and backfill commits large chunks, so the system forces a checkpoint cadence that is tied directly to ledger ingestion rather than WAL growth or reader pressure. Under concurrent `getTransactions` polling, those repeated checkpoints can consume I/O bandwidth and extend tail latency even though the request path was explicitly split to keep read snapshots short; reducing checkpoint frequency or making it conditional on WAL size should lower the periodic latency spikes.

## Trigger

1. Run continuous ingestion with normal per-ledger commits.
2. Generate sustained `getTransactions` traffic for `startLedger=latest`, especially around each close.
3. Compare p95/p99 latency against a version that batches checkpoints or only checkpoints once the WAL crosses a size threshold.

## Target Code

- `cmd/stellar-rpc/internal/db/db.go:249-258` — `postCommit` always executes `PRAGMA wal_checkpoint(TRUNCATE)`.
- `cmd/stellar-rpc/internal/db/db.go:309-368` — every write commit finishes by calling `postCommit(durationMetrics)`.
- `cmd/stellar-rpc/internal/ingest/service.go:190-224` — tip-following ingest commits once per ledger.
- `cmd/stellar-rpc/internal/ingest/service.go:245-273` — backfill commits chunks that can make the checkpoint even more expensive.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:315-405` — request path opens read transactions and batch-reads ledger metas from the same SQLite database.

## Evidence

The checkpoint policy is unconditional and synchronous: auto-checkpointing is disabled, then every ingest commit forces a truncate checkpoint. That means `getTransactions` traffic is guaranteed to overlap with checkpoint work during steady-state ingestion, not just when the WAL actually needs draining. Because new-ledger pollers cluster around the same time ingest commits, the policy is aligned with the endpoint's hottest read window.

## Anti-Evidence

SQLite WAL mode allows readers to proceed concurrently with many writer operations, so the effect may show up mostly in p95/p99 rather than mean latency. If the WAL stays tiny and storage is fast, the checkpoint cost may be modest outside high-concurrency or backfill-heavy scenarios.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the complete write-commit path from `Service.ingest()` through `writeTx.Commit()` to the `postCommit` closure. Confirmed that `PRAGMA wal_checkpoint(TRUNCATE)` executes unconditionally after every single-ledger commit during tip-following (every ~5 seconds). The checkpoint runs AFTER the write transaction has committed and the global cache lock has been released (line 368), so it does not hold any application-level locks during execution. The read path in `getTransactions` opens read-only transactions via `ledgerReader.NewTx()` using `BeginTx(ctx, &sql.TxOptions{ReadOnly: true})` against the same SQLite database. SQLite's `wal_checkpoint(TRUNCATE)` does not block concurrent readers — it checkpoints frames not needed by active readers and only truncates the WAL if all frames are checkpointed. The contention is therefore I/O-level (fsync and page writeback competing with read I/O), not lock-level.

### Code Paths Examined

- `db/db.go:73-89` — `openSQLiteDB` sets `_journal_mode=WAL&_wal_autocheckpoint=0&_synchronous=NORMAL`, confirming auto-checkpoint is explicitly disabled
- `db/db.go:249-258` — `postCommit` closure unconditionally runs `PRAGMA wal_checkpoint(TRUNCATE)` and records duration in `durationMetrics["wal_checkpoint"]`
- `db/db.go:309-368` — `writeTx.Commit()` commits the SQLite transaction (line 343), updates the global cache under lock (lines 340-358), releases the lock, then calls `postCommit` (line 368)
- `ingest/service.go:190-242` — `Service.ingest()` commits once per ledger, triggering one checkpoint per ~5-second ledger close
- `ingest/service.go:245-273` — `Service.ingestRange()` commits 960-ledger chunks during backfill, producing larger WAL checkpoints
- `db/ledger.go:170-185` — `ledgerReader.NewTx()` acquires cache RLock, opens a read-only SQLite transaction, then releases the RLock — no contention with `postCommit`
- `methods/get_transactions.go:315-405` — read path opens a read transaction, then batch-fetches ledger metas in groups of 50

### Findings

The inefficiency is real but its severity is lower than claimed:

1. **The checkpoint is unconditional and synchronous** — every tip-following commit triggers `wal_checkpoint(TRUNCATE)` regardless of WAL size. Since the WAL is truncated after every commit, it only ever contains one ledger's worth of writes (~50KB–5MB depending on transaction density). The checkpoint of such a small WAL is fast (estimated 1–5ms), but the `TRUNCATE` step requires an `fsync` with fixed minimum latency (~0.5–2ms on SSD).

2. **Timing coincides with hot polling** — new-ledger pollers cluster immediately after ledger close, which is when the ingest commit and checkpoint run. This maximizes the probability of I/O contention between the checkpoint's page writeback/fsync and the pollers' read I/O.

3. **No reader blocking** — SQLite's WAL checkpoint does NOT block concurrent readers. Active readers reading from the WAL continue unimpeded. The contention is purely at the storage I/O level (bandwidth and I/O scheduler competition).

4. **Backfill is a non-issue for getTransactions** — backfill runs before tip-following starts (they don't run concurrently), so the larger 960-ledger chunk checkpoints don't affect live request traffic.

5. **Impact assessment** — the checkpoint overhead (1–5ms) represents a small fraction of total `getTransactions` request cost (which includes DB reads, XDR deserialization, and JSON/base64 marshaling — typically 10–100ms+). The impact is primarily on p99 tail latency for requests that overlap with the checkpoint fsync, not on mean latency or throughput. This places the finding at Low severity (<5% improvement) rather than the claimed Medium.

6. **No correctness risk** — WAL checkpointing is purely a durability/performance tradeoff. Delaying or batching checkpoints doesn't affect data integrity (the WAL itself provides crash safety). The only tradeoff is WAL file growth, which can increase read latency slightly as SQLite must check more WAL frames for page overrides.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/db/db.go:249-258` — the `postCommit` closure in `NewTx`
- **Change description**: Replace unconditional `wal_checkpoint(TRUNCATE)` with a conditional checkpoint. Two options:
  - (A) **Size-based**: Check WAL size with `PRAGMA wal_checkpoint` (returns frame count) and only run TRUNCATE when WAL exceeds a threshold (e.g., 1000 frames / ~4MB)
  - (B) **Time-based**: Track last checkpoint time and only checkpoint if N seconds have elapsed (e.g., every 30 seconds)
  - (C) **Switch to PASSIVE**: Use `wal_checkpoint(PASSIVE)` which does not require the WAL to be empty and avoids the TRUNCATE fsync overhead; let the WAL accumulate and periodically run TRUNCATE on a timer
- **Correctness check**: Existing tests in `db/ledger_test.go`, `db/transaction_test.go`, `db/event_test.go` exercise the commit path. The `BenchmarkGetLedgerMetas` benchmark in `ledger_test.go:202` can be extended to measure read latency under concurrent writes.
- **Benchmark focus**: Measure p95/p99 latency of `getTransactions` under concurrent tip-following ingestion. The `durationMetrics["wal_checkpoint"]` already tracks checkpoint duration on the write side. Expected improvement: 1–5ms reduction in p99 tail latency (~2–5% for typical workloads).
