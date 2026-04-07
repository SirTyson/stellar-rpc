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

---

## Review

**Verdict**: VIABLE
**Severity**: Informational
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated (fail/002 examined a different mechanism: cursor advancement within processTransactionsInLedger, not the index-empty case)

### Trace Summary

Traced the complete cursor lifecycle through `getTransactionsByLedgerSequence` (lines 251–381) and `processTransactionsInLedger` (lines 78–209). Confirmed the code defect: when `GetLedgerSequencesWithTransactions` returns an empty slice (line 314), the `if len(ledgerSeqs) > 0` block is skipped entirely, `processTransactionsInLedger` is never called, and `cursor` remains at `toid.New(0, 0, 0)` (line 289). The response emits `cursor.String()` = `"0"` (line 379). A subsequent cursor-only poll with `"0"` passes `ValidatePagination` (cursor-only requests allow `startLedger=0`), `initializePagination` parses it to `toid{0, 1, 0}`, and the next index query scans from ledger 0. However, the trigger scenario described in the hypothesis is inaccurate — polling at the tip after processing transactions does NOT produce a zero cursor.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:289` — `cursor := toid.New(0, 0, 0)` initializes cursor to zero
- `cmd/stellar-rpc/internal/methods/get_transactions.go:304-312` — `GetLedgerSequencesWithTransactions` call; empty result means cursor stays zero
- `cmd/stellar-rpc/internal/methods/get_transactions.go:314` — `if len(ledgerSeqs) > 0` gate; skipped when index returns empty
- `cmd/stellar-rpc/internal/methods/get_transactions.go:352` — `cursor, done, err = h.processTransactionsInLedger(...)` — only called inside the gate
- `cmd/stellar-rpc/internal/methods/get_transactions.go:379` — `Cursor: cursor.String()` emits `"0"` when no ledgers processed
- `cmd/stellar-rpc/internal/methods/get_transactions.go:62` — `*start = toid.Parse(cursorInt)` parses `"0"` back to `toid{0,0,0}`
- `cmd/stellar-rpc/internal/methods/get_transactions.go:65` — `start.TransactionOrder++` → `toid{0, 1, 0}`
- `cmd/stellar-rpc/internal/db/transaction.go:170-187` — `GetLedgerSequencesWithTransactions` SQL query uses `WHERE ledger_sequence >= 0` when startSeq=0, scanning the entire retention window
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/protocols/rpc/get_ledgers.go:90-93` — `ValidatePagination` allows cursor-only requests with `startLedger=0`
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/toid/main.go:143-155` — `ToInt64()` confirms `toid{0,0,0}` encodes as `0`; `String()` returns `"0"`

### Findings

**The code defect is real but the trigger scenario is much narrower than described.**

The hypothesis claims polling at the tip triggers the zero cursor. This is incorrect. When a client polls past the last transaction, the index query `GetLedgerSequencesWithTransactions(ctx, start.LedgerSequence, lastLedgerSeq, limit+1)` still returns the cursor's ledger (which had transactions, since that's where the cursor came from). `processTransactionsInLedger` runs and returns `toid{lastLedger, 0, 1}` or `toid{lastLedger, lastTx, 1}`, never zero.

The zero cursor occurs only when `GetLedgerSequencesWithTransactions` returns a genuinely empty result, which requires **no transactions at all** in the range `[start.LedgerSequence, lastLedgerSeq]`. This happens in narrow scenarios:

1. **First call with explicit `startLedger` in a range with no transactions**: Client calls `getTransactions(startLedger=X)` where ledgers X through latest have no transactions. Gets cursor `"0"`. Subsequent poll with cursor `"0"` replays from ledger 0.
2. **Retention window eviction**: The cursor points to a ledger that was evicted, AND all newer ledgers in the range also have no transactions.
3. **Empty database**: Fresh node with no transactions ingested yet.

On production networks (mainnet/testnet), these scenarios are rare because the retention window always contains many transactions. The performance impact is therefore negligible for typical workloads.

**Severity downgraded from High to Informational.** The >20% latency/RPS threshold for High severity is not met. The defect is real and worth fixing for correctness, but the performance impact on production `getTransactions` workloads is not measurable.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/methods/get_transactions.go:289` — change cursor initialization from `toid.New(0, 0, 0)` to use the `start` position (e.g., `toid.New(start.LedgerSequence, max(start.TransactionOrder-1, 0), 1)`) so that when no ledgers are processed, the cursor reflects the starting position rather than resetting to zero. Alternatively, add a fixup after the `if len(ledgerSeqs) > 0` block: `if len(ledgerSeqs) == 0 { cursor = toid.New(int32(lastLedgerSeq), 0, 1) }` to advance to the end of the scanned window (matching `getEvents` behavior).
- **Change description**: Ensure cursor doesn't reset to zero when the transaction index returns no results. The simplest fix is to initialize cursor to the start position or add a post-loop fixup for the empty case.
- **Correctness check**: `cmd/stellar-rpc/internal/integrationtest/get_transactions_test.go` covers cursor-based pagination. Add a test case that requests a range with no transactions and verifies the returned cursor is not `"0"`.
- **Benchmark focus**: Difficult to benchmark the performance improvement since the trigger is rare. Focus on a correctness test: call `getTransactions` on an empty range, verify cursor ≠ `"0"`, then poll with the returned cursor and verify the query doesn't scan from ledger 0.
