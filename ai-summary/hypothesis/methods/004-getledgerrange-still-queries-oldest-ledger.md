# H004: Every getTransactions Request Still Pays an Oldest-Ledger Query Because the Cache Tracks Only the Latest Bound

**Date**: 2026-04-06
**Subsystem**: methods
**Severity**: Low
**Impact**: latency / DB I/O
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

For the common case where `getTransactions` returns a small page near the latest ledger, range metadata should come from cache without an extra SQL read. A request that only needs to confirm the newest ledger bounds should not fetch and deserialize the oldest ledger on every call.

## Mechanism

`getTransactionsByLedgerSequence` always begins by calling `readTx.GetLedgerRange(ctx)`. Even when the latest ledger sequence and close time are already cached, `getLedgerRangeWithCache` still issues a DB query for the first ledger because the cache only stores `latestLedgerSeq` and `latestLedgerCloseTime`. That leaves a fixed per-request lookup and `LedgerCloseMeta` deserialize on the hot path, which should be noticeable for small, latest-page requests where the rest of the work is intentionally small.

## Trigger

1. Send a high volume of `getTransactions` requests with small limits against the newest ledgers.
2. Keep the page size small enough that most requests are satisfied by one or two ledgers.
3. Compare latency and query counts against a version that also caches the oldest ledger sequence/close time and only refreshes that cache when retention trimming advances the window.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:221-240` — every request calls `GetLedgerRange` before doing any pagination work.
- `cmd/stellar-rpc/internal/db/ledger.go:ledgerReaderTx.GetLedgerRange:60-64` — the transactional reader always defers to `getLedgerRangeWithCache` when latest bounds are cached.
- `cmd/stellar-rpc/internal/db/ledger.go:getLedgerRangeWithCache:223-250` — the helper still queries `MIN(sequence)` and deserializes the oldest ledger's metadata.
- `cmd/stellar-rpc/internal/db/db.go:dbCache:49-54` — the cache stores only latest ledger fields, so the oldest bound is never memoized.

## Evidence

The DB cache type has no oldest-ledger fields, and `getLedgerRangeWithCache` explicitly says it "only needs to look up the first ledger since we have the latest cached." `getTransactions` unconditionally calls that helper even for requests that immediately paginate near the latest ledger, so the extra query is guaranteed on every request.

## Anti-Evidence

This is a fixed-cost optimization, so it matters most when a request is otherwise cheap. Large historical scans or event-heavy JSON pages will still be dominated by ledger walking and serialization, and any oldest-ledger cache has to stay coherent with retention trimming as the window advances.
