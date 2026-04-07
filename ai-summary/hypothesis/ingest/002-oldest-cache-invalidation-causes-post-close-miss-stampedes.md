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
