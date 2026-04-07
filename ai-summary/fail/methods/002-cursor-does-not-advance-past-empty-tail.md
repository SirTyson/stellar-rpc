# H002: `getTransactions` Leaves the Cursor Behind the Empty Tail and Forces Re-Scans

**Date**: 2026-04-07
**Subsystem**: methods
**Severity**: High
**Impact**: latency / RPS / repeated DB work
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When a `getTransactions` scan reaches the current end of the retention window without filling `limit`, the returned cursor should advance to the end of the scanned window. A client that reuses that cursor for polling should only inspect newly ingested ledgers, not rescan the same empty tail after the last returned transaction.

## Mechanism

The handler initializes `cursor` to `toid.New(0, 0, 0)` and only updates it from `processTransactionsInLedger`, which returns the last transaction position touched inside a ledger. If the request exhausts the available window before hitting `limit`, the response still returns the last returned transaction's TOID — or `"0"` when no transactions were found — rather than a cursor representing the latest scanned ledger. Cursor-driven clients therefore restart from an old position and force the server to rescan the same empty suffix on every poll. `getEvents` already has the opposite behavior: when it does not fill the page, it advances the cursor to the end of the search window.

## Trigger

1. Let the latest transaction in the DB be several ledgers behind `LatestLedger`, or use a quiet window with no transactions at all.
2. Call `getTransactions`, then immediately call it again with the previous response's cursor.
3. Compare scanned ledgers and latency against a version that returns an end-of-window cursor when `len(transactions) < limit`.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:242-244` — `cursor` starts at `toid.New(0, 0, 0)`.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:289-301` — the cursor is only updated while processing ledgers that contain readable transactions.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:303-310` — the response always emits `cursor.String()` with no "scan completed" adjustment.
- `cmd/stellar-rpc/internal/integrationtest/get_transactions_test.go:89-100` — the integration flow expects callers to reuse the returned cursor for the next page.
- `cmd/stellar-rpc/internal/methods/get_events.go:219-230` — the sibling endpoint advances the cursor to the end of the scanned window when the page is not full.

## Evidence

There is no branch in `getTransactionsByLedgerSequence` analogous to `getEvents`'s "limit not reached" cursor fix-up. That means a request that returns one old transaction and then scans hundreds of empty ledgers to the tip will hand the client the old transaction cursor, not the tip cursor it just proved was empty.

## Anti-Evidence

Clients that poll by `startLedger = latest+1` instead of by cursor would not benefit. The gain is also smallest on dense networks where the empty tail after the last returned transaction is short.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated
**Failed At**: reviewer

### Trace Summary

Traced the complete cursor lifecycle through `getTransactionsByLedgerSequence` (lines 238-311) and `processTransactionsInLedger` (lines 75-199). The hypothesis's core claim — that the cursor only updates when processing ledgers with transactions — is factually incorrect. The outer `cursor` variable (line 244) is reassigned on EVERY call to `processTransactionsInLedger` (line 290), including for empty ledgers. For empty ledgers, `processTransactionsInLedger` returns `toid.New(ledgerSeqInt32, 0, 1)` (line 111, returned at line 198 after the zero-iteration loop). The cursor therefore always advances to the last scanned ledger, not the last transaction.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:244` — `cursor := toid.New(0, 0, 0)` initializes the outer cursor
- `cmd/stellar-rpc/internal/methods/get_transactions.go:289-297` — `cursor, done, err = h.processTransactionsInLedger(...)` reassigns cursor for EVERY ledger (empty or not)
- `cmd/stellar-rpc/internal/methods/get_transactions.go:111` — `cursor := toid.New(ledgerSeqInt32, 0, 1)` initializes the inner cursor to the current ledger
- `cmd/stellar-rpc/internal/methods/get_transactions.go:112` — `for i := startTxIdx; i <= txCount; i++` — loop does not execute when txCount=0 (empty ledger)
- `cmd/stellar-rpc/internal/methods/get_transactions.go:198` — returns `cursor` (= `toid{ledgerSeq, 0, 1}` for empty ledgers)
- `cmd/stellar-rpc/internal/methods/get_transactions.go:309` — response emits `cursor.String()` which reflects the last scanned ledger
- `github.com/stellar/go-stellar-sdk@v0.4.0/toid/main.go:129-135` — `toid.New` returns a fresh `*ID` each call
- `cmd/stellar-rpc/internal/methods/get_transactions.go:59-62` — `initializePagination` parses cursor and increments TransactionOrder, so next poll starts at TransactionOrder=1 in the last scanned ledger
- `cmd/stellar-rpc/internal/methods/get_events.go:219-230` — getEvents uses MaxCursor for end-of-window, advancing past all events in the last ledger (slightly more aggressive advancement than getTransactions, but the difference is one ledger rescan)

### Why It Failed

The hypothesis is based on an incorrect reading of the code. It claims "the cursor is only updated while processing ledgers that contain readable transactions," but line 290 (`cursor, done, err = h.processTransactionsInLedger(...)`) reassigns the outer `cursor` on every iteration of the inner `for _, ledger := range ledgers` loop. For empty ledgers, `processTransactionsInLedger` creates a fresh cursor at `toid.New(ledgerSeqInt32, 0, 1)` (line 111) and returns it at line 198 after the zero-iteration transaction loop. Therefore, after scanning to `lastLedgerSeq`, the response cursor is `toid{lastLedgerSeq, 0, 1}`, NOT the last transaction's TOID. A client resuming from this cursor starts at `toid{lastLedgerSeq, 1, 1}` (after the TransactionOrder increment in initializePagination), which means at most one ledger is rescanned — not the entire empty tail.

There is a minor difference from getEvents: getEvents uses MaxCursor to fully skip the last scanned ledger on resume, while getTransactions rescans the last ledger once. But this single-ledger rescan is trivially cheap compared to the DB batch fetch cost and does not constitute a measurable performance impact.

### Lesson Learned

When analyzing cursor advancement logic, trace the actual assignment chain (outer variable ← inner function return) rather than inferring behavior from function names. The `processTransactionsInLedger` function name suggests it only processes transactions, but it always returns a cursor reflecting the current ledger's position regardless of transaction count.
