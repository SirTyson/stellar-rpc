# H001: Partial LCM-cache misses force getTransactions to reread already-hot ledgers

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Medium
**Impact**: DB read / XDR decode overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When a `getTransactions` page spans both cached and uncached ledgers, the handler should reuse the cached ledgers and fetch only the misses from SQLite. A page that overlaps the hot tip window should not discard those in-memory ledgers just because one older ledger falls outside the cache.

## Mechanism

The current XDR fast path treats the LCM cache as all-or-nothing. `GetAllLCMs()` returns `false` on any miss, and `getTransactionsByLedgerSequence()` then falls back to `StreamLedgersBySequences()` for the entire selected ledger set, re-reading even the newest ledgers that are already resident in memory. On sparse pages near the tip, that turns a mixed hot/cold request into a full DB scan of the hot suffix, wasting ledger blob reads and `LedgerCloseMeta` deserialization that the process already paid during ingestion.

## Trigger

Issue `getTransactions` in default `xdr` format with a cursor near the tip on a sparse retained window so the selected page needs, for example, 15 ledgers while only the newest 10 are in `lcmCache`. The response will re-fetch all 15 ledgers from SQLite instead of serving the hot 10 from memory and only reading the older 5 misses.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:413-499` — cache use is gated behind a full-hit check, with any miss falling back to DB streaming for all ledgers.
- `cmd/stellar-rpc/internal/ingest/lcm_cache.go:GetAllLCMs:45-77` — returns `(nil, false)` as soon as one requested sequence is outside the cached window.
- `cmd/stellar-rpc/internal/db/ledger.go:StreamLedgersBySequences:253-284` — fallback path re-reads and deserializes every requested ledger from SQLite.

## Evidence

The hot-path cache probe is `if cachedLCMs, ok := h.lcmCache.GetAllLCMs(ledgerSeqs); ok { ... }`, and there is no partial-hit path afterward (`cmd/stellar-rpc/internal/methods/get_transactions.go:418-435`). `GetAllLCMs()` itself is explicitly all-or-nothing (`cmd/stellar-rpc/internal/ingest/lcm_cache.go:49-77`). In the miss case, the handler drops to `StreamLedgersBySequences()` and deserializes the whole requested ledger set, including any hot tip ledgers that had just failed the full-coverage test by only one or two sequences (`cmd/stellar-rpc/internal/methods/get_transactions.go:472-498`).

## Anti-Evidence

Requests fully inside the cache already take the fast path, and requests fully outside the cache see no benefit. This optimization is most valuable for mixed hot/cold pages near the cache boundary, so the win depends on real clients often polling just beyond the newest cached tip range.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the XDR path through `getTransactionsByLedgerSequence` (lines 413-499). Confirmed `GetAllLCMs` (lcm_cache.go:49-77) is strictly all-or-nothing: it computes `firstSeq`/`lastSeq` from the circular buffer and returns `(nil, false)` the instant any requested sequence falls outside `[firstSeq, lastSeq]`. On miss, the handler falls through to `StreamLedgersBySequences` (ledger.go:253-284) which re-reads and XDR-deserializes every requested ledger from SQLite, including those that were already resident in the 10-entry LCM cache. The JSON path is unaffected — it always bypasses the cache (line 418 gates on `request.Format != protocol.FormatJSON`).

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:418-435` — cache fast-path only fires when `GetAllLCMs` returns `ok=true`; no partial-hit fallback exists
- `cmd/stellar-rpc/internal/ingest/lcm_cache.go:49-77` — `GetAllLCMs` iterates requested sequences and returns `(nil, false)` if any sequence is outside `[firstSeq, lastSeq]`; allocates a full result slice eagerly before checking (line 65)
- `cmd/stellar-rpc/internal/methods/get_transactions.go:472-498` — XDR fallback streams ALL `ledgerSeqs` through `StreamLedgersBySequences`, re-reading cached ledgers from SQLite
- `cmd/stellar-rpc/internal/db/ledger.go:253-284` — `StreamLedgersBySequences` does `SELECT meta FROM ledger_close_meta WHERE sequence IN (...)`, scanning each row into `xdr.LedgerCloseMeta` via full XDR deserialization
- `cmd/stellar-rpc/internal/ingest/lcm_cache.go:11` — `defaultLCMCacheSize = 10`; daemon creates cache with `NewLCMCache(0)` using default
- `cmd/stellar-rpc/internal/daemon/daemon.go:196` — confirms cache is created with size 0 (→ default 10)

### Findings

**The inefficiency is real.** `GetAllLCMs` is explicitly all-or-nothing. A request needing sequences `[100, 103, 105, 108, 110, 112]` where the cache holds `[105..114]` will fail the cache check entirely and re-read all 6 ledgers from SQLite, including the 4 that were already cached.

**Scope limitations reduce severity from Medium to Low:**

1. **XDR-only**: The cache path is gated on `request.Format != protocol.FormatJSON` (line 418). The JSON path always goes through `BatchGetLedgersBySequences` for raw bytes needed by Rust FFI. With the benchmark workload using 50/50 JSON/base64 mix, only half of requests can potentially benefit.

2. **Narrow trigger window**: With `defaultLCMCacheSize = 10`, partial misses only occur when the requested page straddles the boundary of the 10 most recent ingested ledgers. Tip-polling clients (small cursors near head) almost always hit fully; older-range queries miss fully. The partial overlap band is narrow.

3. **OS page cache mitigates I/O cost**: Recently ingested LCM blobs are likely still in the OS filesystem page cache, so the SQLite "re-reads" are often memory-to-memory copies rather than disk I/O. The main waste is the XDR `UnmarshalBinary` CPU cost for already-deserialized `LedgerCloseMeta` objects.

4. **Correctness of proposed fix is confirmed**: Cached LCMs are copied by value during `GetAllLCMs` (line 75: `result[i] = bucket.BucketContent`), so a partial-hit method can safely extract cached LCMs under the RLock, release it, then query SQLite for only the missing sequences. The merge preserves ascending order since both sources are sequence-ordered.

### PoC Guidance

- **Target code**:
  - `cmd/stellar-rpc/internal/ingest/lcm_cache.go` — add a `GetPartialLCMs(sequences []uint32) (cached map[uint32]xdr.LedgerCloseMeta, uncached []uint32)` method that returns what's available from the cache and the remaining sequences that need DB fetch
  - `cmd/stellar-rpc/internal/methods/get_transactions.go:437-498` — in the XDR `!servedFromCache` path, try the partial cache before falling through to full DB streaming; process cached and DB-fetched LCMs in sequence order

- **Change description**: Add `GetPartialLCMs` to `LCMCache` that iterates requested sequences under RLock, copies cached entries into a map, and collects uncached sequences into a separate slice. In `getTransactionsByLedgerSequence`, when `GetAllLCMs` returns false, call `GetPartialLCMs`. If some are cached, stream only uncached sequences from DB and merge-process both sources in ascending sequence order using `processTransactionsInLedger`.

- **Correctness check**: Existing `getTransactions` tests (including edge cases around cursor boundaries and page limits) should pass unchanged since the optimization only changes which source provides each LCM, not the processing. The `LCMCache` tests should be extended to cover the new partial-hit method.

- **Benchmark focus**: Measure per-request latency and RPS for XDR-format `getTransactions` requests where the cursor is positioned so the page straddles the cache boundary (e.g., needs 15 ledgers, 10 cached). Expect ~30-60% reduction in XDR deserialization cost for the cached fraction of those requests, translating to <5% overall throughput improvement given the narrow trigger window.
