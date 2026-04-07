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
