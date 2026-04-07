# H002: Partial Hot-Cache Misses Reread Mostly Cached Pages

**Date**: 2026-04-07
**Subsystem**: daemon
**Severity**: Medium
**Impact**: redundant DB reads / XDR deserialization in XDR mode
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

If a `getTransactions` page needs twelve recent ledgers and ten of them are already resident in the hot-ledger cache, the daemon should reuse those ten cached ledgers and fetch only the uncached remainder from SQLite. A page that is "mostly hot" should not fall all the way back to a full DB fetch.

## Mechanism

`LCMCache.GetAllLCMs` is all-or-nothing: one missing sequence causes it to return `(nil, false)`, and `getTransactionsByLedgerSequence` then fetches every requested ledger from SQLite instead of merging cached hits with DB misses. Because the daemon's default cache window is only ten ledgers, sparse recent pages can easily spill past that boundary, so a request that could have reused most of its ledger metas instead rereads and re-unmarshals the whole page. A partial-hit API that returns cached prefixes/suffixes plus the gaps to fetch would preserve most of the hot-window benefit.

## Trigger

Run default/XDR `getTransactions` against recent sparse history where the next page spans slightly more than ten ledgers (for example, 12-15 ledgers with only a few transactions each). Compare the current all-or-nothing behavior to a version that serves cached ledgers in place and fetches only missing sequences from SQLite.

## Target Code

- `cmd/stellar-rpc/internal/daemon/daemon.go:193-197` — daemon instantiates the cache with `ingest.NewLCMCache(0)`, which resolves to a ten-ledger default window.
- `cmd/stellar-rpc/internal/ingest/lcm_cache.go:11-29` — `defaultLCMCacheSize` is `10`.
- `cmd/stellar-rpc/internal/ingest/lcm_cache.go:45-77` — `GetAllLCMs` returns `false` on any miss instead of exposing partial hits.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:409-435` — handler uses cache only on a full hit and otherwise falls back to `BatchGetLedgersBySequences` for every ledger in the page.

## Evidence

The cache lookup returns success only when every requested sequence is present in the ten-ledger ring buffer. The handler does not attempt to split the page into cached and uncached spans, so a single ledger outside the window forces the code onto the same DB path as a totally cold request.

## Anti-Evidence

This optimization only applies to the non-JSON path, and it depends on real traffic frequently being just outside the cache window rather than well inside or far outside it. If recent pages are usually dense enough to fit in ten ledgers, the current cache already captures the hot case.
