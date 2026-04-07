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

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full `getTransactionsByLedgerSequence` flow from cache lookup through DB fallback. Confirmed that `LCMCache.GetAllLCMs` (lcm_cache.go:49-77) is genuinely all-or-nothing: any single sequence outside the 10-ledger `LedgerBucketWindow` returns `(nil, false)`. The handler (get_transactions.go:409-431) then falls through to `BatchGetLedgersBySequences`, which reads every requested ledger from SQLite, performs partial header XDR decode, followed by full `UnmarshalBinary` per chunk (line 481). The proposed partial-hit approach is mechanically correct.

### Code Paths Examined

- `cmd/stellar-rpc/internal/ingest/lcm_cache.go:49-77` — `GetAllLCMs` iterates sequences, returns `(nil, false)` on any miss via early return at lines 68 or 73
- `cmd/stellar-rpc/internal/ledgerbucketwindow/ledgerbucketwindow.go:107-113` — `Get(i)` indexes into the circular buffer; cache stores contiguous window only
- `cmd/stellar-rpc/internal/methods/get_transactions.go:409-496` — binary branch: full cache hit (lines 414-431) or full DB fetch (lines 433-496); no merge path
- `cmd/stellar-rpc/internal/db/ledger.go:164-203` — `BatchGetLedgersBySequences` reads all `meta` blobs, partially decodes headers
- `cmd/stellar-rpc/internal/methods/get_transactions.go:480-486` — XDR path does `UnmarshalBinary` per chunk (the deserialization being wasted for would-be-cached ledgers)
- `cmd/stellar-rpc/internal/config/options.go:362-385` — default and max transaction limit are both 200
- `cmd/stellar-rpc/internal/daemon/daemon.go:196` — cache created with `NewLCMCache(0)` → 10 ledgers

### Findings

**The inefficiency is real.** When a page spans N ledgers and M ≤ 10 are in the cache, the current code discards all M cached LCMs and re-fetches all N from SQLite, plus deserializes all N via `UnmarshalBinary`. A partial-hit implementation would save M SQLite reads and M deserializations.

**The fix is correct and preserves correctness.** The cached `xdr.LedgerCloseMeta` values are identical to what `UnmarshalBinary` would produce — they were the ingested values stored before SQLite write. No caller depends on the "from DB" path specifically; `processTransactionsInLedger` consumes the same `xdr.LedgerCloseMeta` type from either source.

**Impact is bounded by workload pattern.** The partial-hit scenario requires the page to straddle the 10-ledger cache boundary — some sequences inside, some outside. This happens when:
- Tip-polling clients (block explorers, indexers) request recent transactions on moderate-traffic networks (~15-25 txns/ledger → limit=200 spans ~10-13 ledgers)
- On busy networks (40+ txns/ledger), pages fit in fewer ledgers and the cache fully hits
- On quiet networks, pages span far beyond the cache and the proportional savings shrink

**Only applies to XDR format.** Line 414 guards the cache path with `request.Format != protocol.FormatJSON`. JSON requests bypass the cache entirely (they need raw LCM bytes for Rust FFI).

**Severity downgrade from Medium to Low:** While per-request savings can be significant in the partial-hit case (saving 80%+ of DB reads), the frequency of partial hits depends on a narrow workload pattern. On high-throughput networks where RPS matters most, pages are dense and the cache fully hits. The overall throughput impact is estimated at <5%.

**Alternative consideration:** Simply increasing `defaultLCMCacheSize` from 10 to 50 would eliminate most partial-hit scenarios with zero code complexity — though this trades ~5× memory for cached LCMs. A partial-hit API is the more memory-efficient solution.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/ingest/lcm_cache.go` — add a `GetPartialLCMs(sequences []uint32) (hits map[uint32]xdr.LedgerCloseMeta, misses []uint32)` method; `cmd/stellar-rpc/internal/methods/get_transactions.go:409-496` — replace the binary branch with a three-phase flow: (1) get partial cache hits, (2) fetch misses via `BatchGetLedgersBySequences`, (3) merge and process in sequence order
- **Change description**: Add a partial-hit cache API that returns cached LCMs plus the list of uncached sequences. In the handler, fetch only uncached sequences from DB, deserialize those, then iterate all ledgers in order using cached values where available and deserialized chunks where not. Note: the `readTx` cannot be released early in the partial-hit case (only when all ledgers are cached).
- **Correctness check**: Existing tests in `cmd/stellar-rpc/internal/methods/get_transactions_test.go` cover the cache path and the DB path; they should pass without modification since both paths produce identical `xdr.LedgerCloseMeta` values.
- **Benchmark focus**: Measure per-request latency for XDR-format `getTransactions` with limit=200 on a network producing ~15 txns/ledger (so pages span ~13 ledgers). Compare cache-miss rate and SQLite read count before/after. Expect ~5-15% latency reduction for the partial-hit workload pattern, with negligible impact on fully-cached or fully-cold requests.
