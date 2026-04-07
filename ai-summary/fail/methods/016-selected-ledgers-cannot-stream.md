# H003: The Index-Driven Path Still Materializes All Selected Ledgers Before It Can Stop on Page Fill

**Date**: 2026-04-07
**Subsystem**: methods
**Severity**: Medium
**Impact**: latency / allocations / time-to-first-result
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

After the transaction index has chosen a sorted set of candidate ledger sequences, `getTransactions` should be able to consume those ledgers row-by-row inside the read snapshot and stop as soon as the page limit is satisfied. Unused tail ledgers should not need to be copied into Go memory before the handler even starts processing the first selected ledger.

## Mechanism

The current index-driven path still relies on `BatchGetLedgersBySequences()`, which uses `Select` to materialize the full result set into memory before returning to the handler. `getTransactionsByLedgerSequence()` only begins its `for _, chunk := range chunks` loop after every selected ledger blob has already been copied and partially decoded, so low-limit requests can hit `done == true` after the first few ledgers while the remaining selected tail has already been paid for. A tx-scoped `StreamLedgersBySequences()` helper (or equivalent callback-based iterator) would preserve the current planner but let the handler break early and release the read transaction sooner.

## Trigger

1. Issue `getTransactions` requests with small limits against dense or moderately dense recent ledgers so the current `limit+1` sequence heuristic returns more ledgers than the page actually needs.
2. Inspect allocation and CPU profiles around `BatchGetLedgersBySequences()` and the subsequent per-ledger loop.
3. Compare against a version that streams selected rows through the read transaction and stops immediately once `processTransactionsInLedger()` reports `done`.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:314-359` — all selected chunks are fetched first, then processed, and only then can the handler break on `done`.
- `cmd/stellar-rpc/internal/db/ledger.go:162-204` — `BatchGetLedgersBySequences()` uses `Select`, forcing full materialization before the caller can inspect any row.
- `cmd/stellar-rpc/internal/db/ledger.go:256-283` — the non-transactional `StreamLedgerRange()` shows the repository already has a callback-based streaming pattern for row-by-row ledger reads.

## Evidence

The hot path already proves it often stops early: `processTransactionsInLedger()` returns `(cursor, true, nil)` as soon as `len(*txns) >= limit`, but that early-stop signal arrives only after the entire `chunks` slice exists. The DB layer also already contains a working row-streaming design via `Query()` in `StreamLedgerRange()`, so the absence of an equivalent by-sequence iterator looks like an API gap rather than a fundamental SQLite limitation.

## Anti-Evidence

If the planner becomes exact enough that almost every selected ledger is needed, streaming saves less because there is little unused tail to discard. This is therefore complementary to planner improvements rather than a replacement for them.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated (related to H008 and H015 but proposes streaming rather than decode reorganization)
**Failed At**: reviewer

### Trace Summary

Traced the full code path from `getTransactionsByLedgerSequence` through the planner (`GetLedgerSequencesWithTransactions`) and `BatchGetLedgersBySequences` to understand how many ledgers get fetched vs. how many the handler actually processes. The hypothesis's core premise — that "low-limit requests can hit `done == true` after the first few ledgers while the remaining selected tail has already been paid for" — is invalidated by the current row-level precision planner. The planner already applies `LIMIT limit` to individual (ledger_sequence, application_order) transaction rows, not to ledger sequences, so the set of returned ledger sequences is already minimal.

### Code Paths Examined

- `cmd/stellar-rpc/internal/db/transaction.go:171-201` — `GetLedgerSequencesWithTransactions` uses a subquery with `LIMIT limit` on (ledger_sequence, application_order) pairs, then takes `DISTINCT ledger_sequence`. This means it returns exactly the ledger sequences that contain the next `limit` transaction rows, not `limit` ledger sequences.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:304-306` — The planner is called with `int(limit)` (NOT `limit+1` as the hypothesis claims). The hypothesis describes an older heuristic that no longer exists.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:314-359` — The handler iterates `chunks` and can `break` early at line 356-358. But the question is how many unused chunks remain after the break.
- `cmd/stellar-rpc/internal/db/ledger.go:162-204` — `BatchGetLedgersBySequences` materializes all selected blobs via `Select`. Each blob gets a partial header decode (~7-20µs).
- `cmd/stellar-rpc/internal/methods/get_transactions.go:339-351` — Full `UnmarshalBinary` only happens for ledgers that are actually processed in the loop.

### Why It Failed

The hypothesis rests on the premise that the planner returns significantly more ledger sequences than the handler needs, creating a "tail" of fetched-but-unused ledgers that streaming could avoid. This premise is wrong with the current code:

1. **The "limit+1 sequence heuristic" no longer exists.** The hypothesis describes an older code version. The current planner (`GetLedgerSequencesWithTransactions`) uses row-level precision with `LIMIT limit` on individual transaction rows (not ledger sequences), so it returns only the ledger sequences that contain the next `limit` transactions.

2. **Overfetch is near-zero with the row-level planner.** Consider: if `limit=10` and ledger A has 3 txns starting from the cursor and ledger B has 7+ txns, the planner returns `[A, B]` (exactly 2 ledgers). The handler processes A (3 txns), then B (7 txns to reach limit, `done=true`, `break`). Both ledgers were needed. In the worst case, the handler might leave 1 ledger unprocessed if the handler finds slightly more transactions per ledger than expected (e.g., due to TOCTOU between the planner query running outside the read snapshot and the LCM fetch inside it). But this is at most 1 unused ledger, not a significant tail.

3. **The waste from 0-1 unused ledgers is negligible.** One unused ledger costs: ~500KB SQLite blob copy + ~7-20µs partial header decode. On a request that takes 5-50ms total, this is <1% overhead. Full `UnmarshalBinary` (the expensive part at ~100µs-10ms) is NOT paid for unused ledgers because it occurs inside the handler loop which breaks early.

4. **Streaming adds complexity for no measurable gain.** Adding a `StreamLedgersBySequences` method to `LedgerReaderTx` would require a new `Query()`-based path on the transaction session, callback/iterator plumbing, and changes to the handler's error handling. The complexity cost is not justified by saving <1% of request time.

### Lesson Learned

When evaluating materialization overhead, check the precision of the upstream data selection. A row-level precision planner that returns `LIMIT N` transaction rows already minimizes the ledger set to near-optimal. Streaming only provides significant savings when the selection layer is coarse-grained (e.g., selecting by ledger count or range rather than by transaction count), and that coarse-grained approach has already been replaced in this codebase.
