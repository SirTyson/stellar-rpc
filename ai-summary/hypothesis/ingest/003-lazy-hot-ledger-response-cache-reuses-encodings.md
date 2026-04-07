# H003: Ingest-managed recent-ledger lifecycle is unused, so repeated pollers re-encode identical `getTransactions` results after every close

**Date**: 2026-04-07
**Subsystem**: ingest
**Severity**: Medium
**Impact**: CPU / xdr2json / base64 / allocation overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

After a new ledger is ingested, repeated `getTransactions` calls for that same hot recent ledger should be able to reuse already formatted transaction results until the ledger ages out of the recent window. Pollers hitting the newest ledger should not force the server to rebuild identical `TransactionInfo` payloads for every request.

## Mechanism

The ingest subsystem already defines the exact lifecycle of "hot recent ledgers" but currently retains only `latestIngestedSeq`; the request path always starts from raw `LedgerCloseMeta` and reruns `ParseTransaction()`, `transactionToJSON()`, `jsonifySlice()`, event encoding, and base64 conversion for every hit. A recent-ledger cache owned by the ingest window could lazily memoize per-ledger `[]protocol.TransactionInfo` (or per-format variants) on the first request, then reuse those slices for subsequent pollers until ingest appends new ledgers and evicts the old ones. That avoids the flaw in prior "precompute on ingest" ideas because the expensive formatting work is still paid only when a ledger is actually queried.

## Trigger

1. Run normal tip-following ingest.
2. After each ledger close, issue many `getTransactions` requests against the newest ledger or a very small recent range, especially with `format=json`.
3. Compare the current path against a version that lazily memoizes per-ledger responses in an ingest-evicted recent window.

## Target Code

- `cmd/stellar-rpc/internal/ingest/service.go:ingest:190-242` — ingest establishes the hot-ledger frontier but retains only the latest sequence.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:139-227` — every request rebuilds `TransactionInfo` and re-encodes results/events.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:304-368` — repeated pollers always walk ledgers again; there is no recent-ledger memoization layer.
- `cmd/stellar-rpc/internal/db/transaction.go:ParseTransaction:238-321` — request path repeatedly marshals transaction result/meta/envelope/events to bytes.
- `cmd/stellar-rpc/internal/methods/json.go:transactionToJSON/jsonifySlice/jsonifySliceOfSlices:12-90` — JSON requests repeatedly cross the FFI boundary and rebuild identical JSON payloads.
- `cmd/stellar-rpc/internal/methods/get_transaction.go:BuildEventsXDRFromTransaction/BuildEventsJSONFromTransaction:133-155` — event formatting is also repeated per request.
- `cmd/stellar-rpc/internal/ledgerbucketwindow/ledgerbucketwindow.go:25-120` — existing bounded recent-ledger container is suitable for cache ownership and eviction.
- `cmd/stellar-rpc/internal/daemon/daemon.go:220-225` — backfill already resets caches, giving a clean invalidation boundary for a hot-ledger response cache.

## Evidence

There is no memoization layer anywhere in the `getTransactions` path: each request reconstructs the same response objects from scratch even when many clients are polling the same latest ledger immediately after close. The codebase already has the ingredients to make such caching safe — contiguous recent-ledger windows, explicit backfill reset points, and a clear ingest-driven eviction boundary — but it currently uses those only for scalar metadata and fee stats.

## Anti-Evidence

The first request for a ledger sees no benefit, and memory usage could become significant for dense ledgers or if both JSON and XDR variants are cached. The cache design must preserve cursor semantics and avoid copying very large response slices unnecessarily.
