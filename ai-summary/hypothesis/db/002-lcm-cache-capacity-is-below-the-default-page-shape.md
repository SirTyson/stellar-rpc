# H002: The tip LCM cache is too small to satisfy the default getTransactions page

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Medium
**Impact**: DB read / XDR decode overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

The built-in tip cache should be sized so the common default `getTransactions` page can actually hit it on the retained workload. If the server advertises a default page of 50 transactions, a sparse-history deployment should not configure a hard-coded cache that can hold only 10 recent ledgers.

## Mechanism

`daemon.MustNew()` constructs `NewLCMCache(0)`, which resolves to a fixed 10-ledger window, while the default `getTransactions` limit is 50. On the repository's retained futurenet workload, prior DB investigations recorded roughly one indexed transaction per ledger, so a default page typically needs about 50 distinct ledgers. Because the current fast path requires every selected ledger to be cached, the 10-ledger cache is structurally too small to activate for the common default page shape, leaving the optimization effectively unreachable unless callers manually lower the limit.

## Trigger

Run default-limit `getTransactions` polling (`limit=50` by default) against a sparse retained window where most ledgers have 0-1 transactions. The handler's planner will select about 50 ledger sequences, but `lcmCache` can hold only the newest 10, so `GetAllLCMs()` will miss every time and the request will drop back to SQLite.

## Target Code

- `cmd/stellar-rpc/internal/daemon/daemon.go:193-197` — constructs the cache with `ingest.NewLCMCache(0)`.
- `cmd/stellar-rpc/internal/ingest/lcm_cache.go:11-29` — default cache size is hard-coded to 10 ledgers.
- `cmd/stellar-rpc/internal/config/options.go:360-370` — default `getTransactions` page size is 50 transactions.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:403-419` — fast path requires full coverage of all planned ledgers.

## Evidence

The cache size and default page size are visibly mismatched in source: `defaultLCMCacheSize = 10` versus `default-transactions-limit = 50` (`cmd/stellar-rpc/internal/ingest/lcm_cache.go:11-29` and `cmd/stellar-rpc/internal/config/options.go:360-370`). The current handler only attempts the cache after the planner has already produced the full ledger set for the page, and `GetAllLCMs()` succeeds only if every one of those ledgers is present (`cmd/stellar-rpc/internal/methods/get_transactions.go:403-419`, `cmd/stellar-rpc/internal/ingest/lcm_cache.go:49-77`). Recent retained-workload notes in `ai-summary/fail/db/015-ledger-json-fast-path-ignores-page-bounds.md` also recorded `avg_tx_per_ledger ≈ 1` and `max_tx_per_ledger = 2`, which makes a 10-ledger cache far smaller than a typical 50-transaction page on the benchmark dataset.

## Anti-Evidence

This matters far less on dense ledgers, on custom deployments that raise transaction density, or for clients that consistently request tiny limits. Increasing the cache also increases resident memory, so the improvement needs to outweigh the extra in-process footprint.
