# H002: Empty Indexed Pages Reset the Cursor to Zero and Replay History

**Date**: 2026-04-07
**Subsystem**: methods
**Severity**: High
**Impact**: latency / repeated DB work / duplicate responses
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When `getTransactions` reaches a quiet tail and finds no newer transactions, the response cursor should stay at the caller's starting position or advance to the end of the scanned window. A polling client that reuses that cursor should inspect only newly ingested ledgers, not restart from ledger 0.

## Mechanism

The current index-driven path initializes `cursor` to `toid.New(0, 0, 0)` and only updates it while processing fetched ledgers. If `GetLedgerSequencesWithTransactions()` returns no sequences, the handler skips the loop entirely and returns cursor `"0"`. `ValidatePagination()` allows cursor-only requests with `startLedger=0`, and `initializePagination()` parses `"0"` into `toid{ledger=0, tx=0, op=0}`; the next poll therefore queries the transaction index from ledger 0 through the latest retained ledger and can replay old transactions instead of staying at the tip.

## Trigger

1. Read to the current tip so that the next `getTransactions` poll has no new transactions to return.
2. Call `getTransactions` again and observe that it returns an empty `transactions` list with cursor `"0"`.
3. Reuse that cursor on the next poll and compare the work done against a version that preserves the starting cursor or advances to the latest scanned ledger.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:286-289` — initializes the response cursor to zero.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:301-360` — does not update the cursor when the indexed ledger list is empty.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:373-380` — always serializes `cursor.String()` into the response.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:43-71` — reuses a returned cursor by parsing it into the next `start`.
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/protocols/rpc/get_ledgers.go:75-104` — pagination validation explicitly allows cursor-only requests with `startLedger=0`.
- `cmd/stellar-rpc/internal/integrationtest/get_transactions_test.go:89-100` — the integration flow expects clients to reuse the previous response cursor.

## Evidence

In the old contiguous-scan code, empty ledgers still advanced the cursor because the handler walked them explicitly. The new index-driven path skips empty tails altogether, but it kept the old "cursor updates inside the per-ledger loop" structure. That means the no-hit case now falls out of the function with the initialization value, which is a zero TOID rather than the caller's previous position.

## Anti-Evidence

Clients that ignore cursors and always send an explicit `startLedger` do not trigger this path. The performance damage is also smallest on nodes whose retention window contains almost no historical transactions, because restarting from zero has less history to replay.
