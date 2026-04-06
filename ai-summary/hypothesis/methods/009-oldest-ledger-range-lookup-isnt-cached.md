# H009: Every getTransactions Request Re-Reads the Oldest Ledger Metadata Just to Validate the Range

**Date**: 2026-04-06
**Subsystem**: methods
**Severity**: Low
**Impact**: latency / DB I/O
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

After the retention window is established, `getTransactions` should usually obtain both the newest and oldest ledger bounds from in-memory cache, not by fetching and decoding the oldest `LedgerCloseMeta` blob on every request before any transaction work begins.

## Mechanism

`getTransactions` always opens a read transaction and calls `GetLedgerRange`. Even on the "cached" path, the DB layer only caches the latest ledger info, so `getLedgerRangeWithCache` still issues `SELECT meta ... MIN(sequence)` and fully deserializes that oldest ledger's metadata to recover `FirstLedger` sequence and close time. The write path already serializes retention trimming and commit order, so it could refresh an oldest-ledger cache once per new ledger (or once per trim advance) instead of forcing every request to pay that fixed DB read and XDR decode.

## Trigger

1. Send a high volume of small `getTransactions` requests near the tip, such as `limit=1` or `limit=10`.
2. Measure the share of latency spent before the first page ledger is fetched.
3. Compare against a prototype that caches `FirstLedger` in `dbCache` and refreshes it during commit/trim rather than during each read request.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:221-240` — every request asks the DB for `GetLedgerRange` before pagination work begins.
- `cmd/stellar-rpc/internal/db/ledger.go:60-65` — the read-transaction cache only shortcuts the latest-ledger half of the range.
- `cmd/stellar-rpc/internal/db/ledger.go:223-250` — the "cached" range path still queries and decodes the oldest ledger metadata blob.
- `cmd/stellar-rpc/internal/db/db.go:49-67` — `dbCache` stores only latest-ledger data today.
- `cmd/stellar-rpc/internal/db/db.go:301-352` — commit/trim is the natural place to refresh oldest-ledger cache once per new ledger.

## Evidence

The request path always needs oldest/latest ledger data for validation and response fields, but only the latest side is cached. That leaves a fixed per-request cost even for tiny near-tip reads that otherwise touch just one ledger. Since writes happen once per ledger close while reads may happen many times between closes, moving this lookup off the request path has strong amortization.

## Anti-Evidence

Large sparse scans will still be dominated by per-ledger fetch/decode work, so this mainly improves the small-request common case. The cache must be invalidated carefully if the oldest retained ledger changes due to anything other than the normal trim-on-commit flow.
