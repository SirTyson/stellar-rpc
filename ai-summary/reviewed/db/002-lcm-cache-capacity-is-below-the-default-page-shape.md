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

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated. Related but distinct from reviewed/db/001-partial-lcm-cache-fallback-rereads-hot-ledgers.md (which addresses partial-miss fallback, not cache sizing).

### Trace Summary

Traced the full cache fast path from `getTransactionsByLedgerSequence` through `GetAllLCMs` and the fallback `StreamLedgersBySequences` path. Confirmed that `defaultLCMCacheSize = 10` is hard-coded, `DefaultTransactionsLimit = 50`, and on the benchmark's sparse workload (~1 tx/ledger), a default page request invokes the planner which returns ~50 distinct ledger sequences. `GetAllLCMs` checks every sequence against `[firstSeq, lastSeq]` and fails immediately when any falls outside the 10-ledger window, causing 100% cache miss rate on default-limit non-JSON requests.

### Code Paths Examined

- `cmd/stellar-rpc/internal/ingest/lcm_cache.go:11` — `defaultLCMCacheSize = 10`, used when `NewLCMCache(0)` is called
- `cmd/stellar-rpc/internal/ingest/lcm_cache.go:49-77` — `GetAllLCMs` computes `firstSeq`/`lastSeq` from window bounds and returns `false` if any requested sequence is out of range
- `cmd/stellar-rpc/internal/daemon/daemon.go:196` — `NewLCMCache(0)` passes 0, triggering the default of 10
- `cmd/stellar-rpc/internal/config/options.go:366-369` — `DefaultTransactionsLimit` defaults to `uint(50)`
- `cmd/stellar-rpc/internal/methods/get_transactions.go:400-411` — `GetLedgerSequencesWithTransactions` planner returns ~50 distinct ledger sequences on sparse workloads
- `cmd/stellar-rpc/internal/methods/get_transactions.go:418-434` — cache fast path: skipped for JSON; for non-JSON, attempts `GetAllLCMs(ledgerSeqs)` which fails when ledgerSeqs spans >10 sequences
- `cmd/stellar-rpc/internal/methods/get_transactions.go:472-498` — non-JSON fallback: `StreamLedgersBySequences` does per-row SQLite reads with full `xdr.LedgerCloseMeta.Scan` → `UnmarshalBinary` for each of ~50 rows
- `cmd/stellar-rpc/internal/db/ledger.go:253-284` — `StreamLedgersBySequences` builds `WHERE sequence IN (...)`, iterates rows calling `q.Scan(&closeMeta)` (full XDR decode) per row

### Findings

1. **The mismatch is confirmed and structural.** With `defaultLCMCacheSize = 10` and the default page needing ~50 ledgers on sparse workloads, the cache window covers only 20% of the required sequences. `GetAllLCMs` is all-or-nothing and fails on the first out-of-range sequence, making the cache optimization dead code under default configuration.

2. **The fix is trivial.** Increasing `defaultLCMCacheSize` from 10 to ~65 (50 default limit + margin) or making it configurable would enable cache hits for tip-polling clients. Memory cost is modest: with sparse ledgers (~1 tx, few KB per LCM), 65 cached LCMs adds <1 MB of resident memory.

3. **Only non-JSON requests benefit.** The cache fast path is gated by `request.Format != protocol.FormatJSON` (line 418), since the JSON path needs raw LCM bytes for the Rust FFI. The benchmark uses a 50/50 JSON/base64 mix, so at most 50% of requests could benefit.

4. **The work saved is real but bounded.** When the cache misses, the fallback is `StreamLedgersBySequences`, which executes a SQL query with 50 values in the `WHERE IN` clause and calls `q.Scan(&closeMeta)` (full `UnmarshalBinary`) for each row. With warm SQLite page cache and small sparse LCMs, this is fast but not free — the XDR decode allocates the full LCM tree for each row. The cache path replaces this with array index lookups into pre-decoded in-memory structures.

5. **Severity downgrade rationale.** Downgraded from Medium to Low because: (a) only 50% of benchmark traffic (non-JSON) can benefit; (b) sparse LCMs are small and fast to decode from warm SQLite; (c) prior benchmark results (fail/db/011) showed that optimizations in the LCM read/decode path had limited end-to-end impact, suggesting this isn't the dominant cost. The total request time includes planner query, response serialization, and network overhead that are unaffected. Estimated end-to-end improvement is <5% on the blended benchmark.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/ingest/lcm_cache.go:11` — change `defaultLCMCacheSize` from `10` to `65` (or derive it from `DefaultTransactionsLimit + margin`). Alternatively, add a config option similar to how `NewLCMCache(size)` already accepts a custom size, and wire it through `daemon.MustNew`.
- **Change description**: Increase the default LCM cache capacity so it covers the default `getTransactions` page on sparse workloads. The simplest change is `const defaultLCMCacheSize = 65`. A slightly better approach is to derive it from the configured `DefaultTransactionsLimit` and pass it to `NewLCMCache` in `daemon.go:196`.
- **Correctness check**: Existing tests in `cmd/stellar-rpc/internal/methods/get_transactions_test.go` and `cmd/stellar-rpc/internal/ingest/lcm_cache_test.go` should pass unchanged. The cache is append-only and contiguity-checked, so increasing capacity cannot introduce correctness issues.
- **Benchmark focus**: Compare `getTransactions` latency with `format=base64` specifically (to isolate the cache benefit from the JSON path). Measure cache hit rate before and after the change using a simple counter or log. Expect a small (<5%) latency improvement on the blended 50/50 benchmark, potentially more visible on base64-only benchmarks.
