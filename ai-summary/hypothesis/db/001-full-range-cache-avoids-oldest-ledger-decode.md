# H001: Full-range cache avoids decoding the oldest retained ledger on every request

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Low
**Impact**: DB / XDR decode / allocation overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

`getTransactions` should answer the retained ledger window metadata from an in-memory range cache, or at worst from a header-only read. A steady-state request with a warm DB should not fully deserialize the oldest retained `LedgerCloseMeta` blob just to populate `oldestLedger`, `oldestLedgerCloseTimestamp`, and request validation bounds.

## Mechanism

`getTransactionsByLedgerSequence()` always calls `readTx.GetLedgerRange()` before it scans any ledgers. Even when the latest ledger is cached, `getLedgerRangeWithCache()` still executes `SELECT meta ... MIN(sequence)` and scans the row into `[]xdr.LedgerCloseMeta`, which fully unmarshals the oldest retained LCM just to read its sequence and close time. The write path already knows the retention cutoff at commit time, so extending the cache to track the first retained ledger (or reusing the existing header-only decode path) should eliminate this unconditional per-request decode.

## Trigger

Run repeated `getTransactions` tip-polling requests with a small limit (`1-10`) against a warm retained window. A CPU/alloc profile should show `GetLedgerRange()` work before any page-specific ledger scan, even when the returned page is satisfied by the first current ledger.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:206-233` — every request resolves the ledger range before pagination and scanning
- `cmd/stellar-rpc/internal/db/ledger.go:ledgerReaderTx.GetLedgerRange:61-65` — read transaction always routes through the range helpers
- `cmd/stellar-rpc/internal/db/ledger.go:getLedgerRangeWithCache:246-275` — cached path still selects and fully scans the oldest `meta` blob
- `cmd/stellar-rpc/internal/db/db.go:writeTx.Commit:301-352` — commit already computes retention trimming and updates cache under lock

## Evidence

The handler opens a read transaction and immediately calls `GetLedgerRange()` before it has processed any ledger data (`cmd/stellar-rpc/internal/methods/get_transactions.go:206-233`). The "cached" range path in `ledger.go` only avoids fetching the latest ledger; it still runs `sq.Select("meta")` for `MIN(sequence)` and scans into `[]xdr.LedgerCloseMeta`, which forces a full XDR unmarshal of the oldest retained blob (`cmd/stellar-rpc/internal/db/ledger.go:246-275`). There is already a dedicated benchmark for this path (`cmd/stellar-rpc/internal/db/ledger_test.go:187-197`), and the DB write path already has the information needed to advance the retained-window head during trim (`cmd/stellar-rpc/internal/db/db.go:301-352`).

## Anti-Evidence

If a request already scans and deserializes many ledgers or transactions, one extra oldest-ledger decode will be amortized. Any fix must preserve snapshot-consistent range reporting when the retention window advances, especially across startup cache misses and post-trim commits.
