# H003: Empty Ledgers Still Build an Ingest Reader Before the Handler Learns There Is Nothing to Read

**Date**: 2026-04-06
**Subsystem**: methods
**Severity**: Medium
**Impact**: CPU / allocations
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When a scanned ledger contains zero transactions, `getTransactions` should skip directly to the next ledger without constructing an `ingest.LedgerTransactionReader`. Empty-ledger scans should be almost free once the metadata is loaded.

## Mechanism

`processTransactionsInLedger` calls `ingest.NewLedgerTransactionReaderFromLedgerCloseMeta` before it checks `ledger.CountTransactions()`. For empty ledgers, the function then sets `txCount := ledger.CountTransactions()` and immediately skips the loop, meaning reader construction and any internal setup/allocation work were pure overhead. The pattern is avoidable: `db/event.go` already guards on `txCount == 0` before opening a transaction reader for event ingestion.

## Trigger

1. Load a retention window with a large run of empty ledgers.
2. Call `getTransactions` with `startLedger` before that run and a small limit so the handler must traverse it.
3. Compare CPU samples or allocation counts before and after moving the `CountTransactions()` fast path ahead of reader construction.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:99-130` — reader construction happens before the zero-transaction check.
- `cmd/stellar-rpc/internal/db/event.go:76-85` — event ingestion shows an existing `txCount == 0` guard before opening the reader.

## Evidence

The current ordering is visible directly in the hot path: reader creation at line 104, `txCount := ledger.CountTransactions()` at line 128, and the read loop only starts at line 130. A sibling subsystem already treats zero-transaction ledgers as a cheap early-return case, which suggests the same fast path was simply missed here.

## Anti-Evidence

This helps only when the scanned range contains many empty ledgers; dense ledgers still need the reader. It also does not address the per-ledger SQL lookup cost, so its benefit compounds best when paired with a better ledger-selection strategy.
