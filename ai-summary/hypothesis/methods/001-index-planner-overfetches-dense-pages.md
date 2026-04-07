# H001: Index Planner Overfetches Dense `getTransactions` Pages

**Date**: 2026-04-07
**Subsystem**: methods
**Severity**: High
**Impact**: latency / DB I/O / allocations
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once `getTransactions` knows the request limit and starting cursor, it should fetch only the non-empty ledgers needed to fill the page. A dense page such as `limit=1`, `limit=10`, or even `limit=200` should not prefetch `limit+1` distinct ledgers when the first one or two ledgers already contain enough transactions.

## Mechanism

The current index-driven planner bounds `GetLedgerSequencesWithTransactions()` by `int(limit)+1`, which is a transaction-count heuristic applied to **ledger** selection. On dense traffic, one selected ledger can contribute dozens or hundreds of transactions, yet `BatchGetLedgersBySequences()` still copies and partially decodes every selected ledger before `processTransactionsInLedger()` gets a chance to stop on the first full page. That reintroduces dense-page overfetch under the new planner shape and can easily dominate low-limit requests near the tip.

## Trigger

1. Populate recent ledgers densely enough that one or two ledgers satisfy a page (for example, 50-100 transactions per ledger).
2. Call `getTransactions` with `startLedger` near the latest ledger and `limit=1`, `limit=10`, or `limit=200`.
3. Compare latency and bytes allocated against a version that probes a small number of non-empty ledgers first and only expands if the page is still short.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:301-359` — queries up to `limit+1` distinct ledgers, fetches all of them, and only then starts per-ledger processing.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:203-205` — the page can become complete inside the first processed ledger, after the prefetch has already happened.
- `cmd/stellar-rpc/internal/db/transaction.go:GetLedgerSequencesWithTransactions:170-187` — limits the planner by count of ledger sequences rather than by actual remaining transactions.
- `cmd/stellar-rpc/internal/db/ledger.go:BatchGetLedgersBySequences:162-204` — materializes every selected ledger blob and partially decodes its header before the caller can stop early.

## Evidence

The old dense-page overfetch issue was tied to a fixed contiguous batch; the current code has the same shape with a different planner. The request asks for transactions, but the planner selects up to `limit+1` **ledgers** without consulting cursor position inside the first ledger or observed transaction density, so a `limit=200` page can still fetch 201 non-empty ledger blobs even when only 2 ledgers are actually needed.

## Anti-Evidence

Sparse one-transaction-per-ledger workloads genuinely need close to `limit` distinct ledgers, so the current planner is well matched there. The gain is concentrated on dense ledgers and small/medium limits, not on the sparse historical scan workload that motivated the index-driven redesign.
