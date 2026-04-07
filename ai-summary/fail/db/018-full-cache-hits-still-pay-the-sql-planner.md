# H004: Full cache hits still open SQLite and run the planner before serving from memory

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Low
**Impact**: DB setup / index-query overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When a request is fully satisfiable from the hot in-memory window, `getTransactions` should avoid opening a SQLite read transaction and querying the transaction index just to rediscover that the needed ledgers are already in memory. Hot-cache hits should be as close as possible to pure in-process page assembly.

## Mechanism

The handler always creates `readTx`, resolves the ledger range, validates the request, and runs `GetLedgerSequencesWithTransactions()` before it even checks `lcmCache`. For small repeated polls wholly inside the cached tip window, those SQL operations become a fixed per-request tax even though the cache already contains contiguous ledgers and the XDR path can often reuse the envelope hash maps too. A cache-aware in-memory planner for the hot window could satisfy these requests without touching SQLite at all.

## Trigger

Repeatedly call `getTransactions` in default `xdr` format with `limit=1..10` and a cursor wholly inside the newest cached ledgers. Profiles should still show `BeginTx`, `GetLedgerRange`, and the planner query on every request even though the later ledger payloads all come from `lcmCache`.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:353-419` — SQLite transaction and planner query happen before the cache check.
- `cmd/stellar-rpc/internal/ingest/lcm_cache.go:GetAllLCMs:45-77` — cached ledgers are contiguous and ordered, which makes an in-memory hot-window scan feasible.
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:newLedgerTransactionReader:37-61` — envelope-cache hits can already remove much of the per-ledger setup once the ledger payload itself comes from memory.

## Evidence

The call order is explicit: `NewTx()` → `GetLedgerRange()` → `request.IsValid()` → `initializePagination()` → `GetLedgerSequencesWithTransactions()` → only then `GetAllLCMs()` (`cmd/stellar-rpc/internal/methods/get_transactions.go:353-419`). `GetAllLCMs()` exposes the cached ledgers as an ordered contiguous window, so a small in-memory scan over those ledgers could discover the same page boundary for full hot-window hits without the SQLite planner round-trip (`cmd/stellar-rpc/internal/ingest/lcm_cache.go:57-77`). Because `newLedgerTransactionReader()` also short-circuits on an `envelopeCache` hit, the remaining cost on such requests can collapse enough that the planner query becomes a visible fixed overhead (`cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:37-61`).

## Anti-Evidence

This only helps requests wholly inside the hot cache window; partial or cold requests still need SQLite. The existing planner query is already much cheaper than reading ledger blobs, so this is likely a low-severity optimization unless the deployment has a very high rate of tiny repeated polls.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated (related to H007/H012/H017 but distinct in proposing full SQLite bypass for cache hits)
**Failed At**: reviewer

### Trace Summary

Traced the full `getTransactionsByLedgerSequence` flow from line 353 through 500. Confirmed the handler opens a SQLite read transaction (`NewTx` at line 353), gets ledger range from the in-tx cache snapshot (line 364, free in steady state), validates and paginates (lines 372-383, pure in-memory), runs the planner query `GetLedgerSequencesWithTransactions` (line 403, one B-tree index scan), and only then checks the LCM cache at line 418. The overhead being proposed for elimination is `BeginTx` + planner query + eventual `Done()` rollback — approximately 100–300µs total per request.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:353-360` — `h.ledgerReader.NewTx(ctx)` opens a read-only SQLite transaction via `Clone() + BeginTx(ReadOnly: true)`
- `cmd/stellar-rpc/internal/db/ledger.go:290-305` — `NewTx` clones the session, begins tx, snapshots cache values under RLock
- `cmd/stellar-rpc/internal/db/ledger.go:75-93` — `GetLedgerRange` on the read tx returns immediately from cache when both bounds are populated (the normal steady-state path) — zero SQL
- `cmd/stellar-rpc/internal/db/transaction.go:171-201` — `GetLedgerSequencesWithTransactions` runs a `SELECT DISTINCT ledger_sequence FROM (subquery) ... LIMIT ?` against the `transactions` table's `index_ledger_sequence` B-tree
- `cmd/stellar-rpc/internal/methods/get_transactions.go:413-435` — cache probe: `GetAllLCMs(ledgerSeqs)` checks if all planner-returned sequences are in the 10-ledger window; if yes, processes from memory and closes the read tx early
- `cmd/stellar-rpc/internal/ingest/lcm_cache.go:11,49-78` — `defaultLCMCacheSize = 10`; `GetAllLCMs` is all-or-nothing, O(n) in the requested set with O(1) per-element access

### Why It Failed

Three converging lines of evidence show the removable overhead is below the measurement threshold:

1. **BeginTx overhead is negligible**: H007 independently investigated the per-request read-transaction setup cost (`Clone + BeginTx`) and concluded it is dwarfed by subsequent work. SQLite WAL-mode read-only `BEGIN` is a single lightweight call (~10–50µs).

2. **Planner query overhead is negligible**: H012's end-to-end benchmarks at 300/500/700 RPS showed p50 latencies of ~4.5–5.5ms. Eliminating ~100–200µs of planner overhead would represent a ~3–4% change, but H012's measurements showed that even larger planner modifications produced zero measurable gain — changes were lost in benchmark noise. H017 separately confirmed that reducing SQL statement count does not move the endpoint needle.

3. **Narrow applicability limits practical impact**: The optimization only helps XDR-format requests whose cursor range fits entirely within the 10-ledger cache window. JSON requests (a significant fraction of traffic) cannot use the LCM cache at all because they need raw bytes for the Rust FFI path. Default-limit requests (limit=50) on the retained benchmark workload (avg ~1 tx/ledger) require ~50 ledgers, structurally exceeding the 10-ledger cache. Only tiny tip-polling requests (limit=1–10) at the very latest ledger range would benefit — a narrow traffic slice where the absolute savings (~200µs) are already small relative to transaction processing time (~1–3ms).

### Lesson Learned

Per-request fixed SQL overhead (BeginTx + lightweight index queries) in `getTransactions` has been independently investigated by three related hypotheses (H007, H012, H017) and consistently found to be below the measurement threshold. Future DB-layer optimizations should target reducing blob I/O, XDR decode work, or repeated CPU-heavy per-transaction processing — not statement-level or transaction-setup overhead.
