# H005: Per-ledger `wal_checkpoint(TRUNCATE)` inflates `getTransactions` tail latency during hot polling

**Date**: 2026-04-07
**Subsystem**: ingest
**Severity**: Medium
**Impact**: SQLite I/O contention / tail latency spikes
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

`getTransactions` pollers hitting the newest ledger should not be forced to overlap with a full WAL checkpoint on every ingest commit. The endpoint should benefit from WAL’s read/write decoupling instead of repeatedly paying for checkpoint-driven file truncation and sync work at the exact moment new-ledger traffic arrives.

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

The checkpoint policy is unconditional and synchronous: auto-checkpointing is disabled, then every ingest commit forces a truncate checkpoint. That means `getTransactions` traffic is guaranteed to overlap with checkpoint work during steady-state ingestion, not just when the WAL actually needs draining. Because new-ledger pollers cluster around the same time ingest commits, the policy is aligned with the endpoint’s hottest read window.

## Anti-Evidence

SQLite WAL mode allows readers to proceed concurrently with many writer operations, so the effect may show up mostly in p95/p99 rather than mean latency. If the WAL stays tiny and storage is fast, the checkpoint cost may be modest outside high-concurrency or backfill-heavy scenarios.
