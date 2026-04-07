# H001: Fixed 50-Ledger Batch Planning Overfetches Dense `getTransactions` Pages

**Date**: 2026-04-07
**Subsystem**: methods
**Severity**: Medium
**Impact**: latency / DB I/O / allocations
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When a `getTransactions` page can be satisfied by the current ledger or the next few ledgers, the handler should fetch and decode only that small prefix. A request such as `limit=1` or `limit=5` against dense recent ledgers should not deserialize a hardcoded 50-ledger window before it knows the first ledger already fills the page.

## Mechanism

`getTransactionsByLedgerSequence` hardcodes `const batchSize = 50` and always calls `readTx.BatchGetLedgerMetas(batchStart, batchEnd)` before it inspects even the first ledger in that batch. `BatchGetLedgerMetas` fully scans every returned row into `[]xdr.LedgerCloseMeta`, so dense or low-limit requests can return after parsing ledger 1 while the other 49 ledgers in the batch were already read, deserialized, and allocated for no reason. An adaptive first-batch planner (for example, probe 1-4 ledgers first, then grow geometrically only if the page is still short) should cut that wasted front-load on the common "small page near the tip" workload.

## Trigger

1. Populate recent ledgers densely enough that one ledger can satisfy most of a page.
2. Issue `getTransactions` with `startLedger` near the latest ledger and `limit` in the 1-10 range.
3. Compare p50 latency and allocations against a version that starts with a tiny first batch and only expands when the page is still not full.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:238-299` — hardcoded `batchSize = 50` and eager `BatchGetLedgerMetas` call before any ledger in the batch is processed.
- `cmd/stellar-rpc/internal/db/ledger.go:118-140` — `BatchGetLedgerMetas` fully deserializes every ledger meta in the requested range into a slice.

## Evidence

The planner does not consult `limit`, the cursor position within the starting ledger, or any observed transaction density before choosing 50 ledgers. The batch is fetched and fully materialized first, then `processTransactionsInLedger` may stop on the first ledger that satisfies the request, which means the tail of the batch was pure overfetch.

## Anti-Evidence

Large sparse scans benefit from wider batches, so the fix should not simply shrink the constant globally. The win is strongest for dense recent traffic and small page sizes; full historical scans that genuinely need many ledgers will amortize the current batch better.
