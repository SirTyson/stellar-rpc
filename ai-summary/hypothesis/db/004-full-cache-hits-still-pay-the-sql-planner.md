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
