# H004: Cursor Resume Might Re-Read Earlier Transactions in the Ledger Before Returning the Next Page

**Date**: 2026-04-06
**Subsystem**: methods
**Severity**: Medium
**Impact**: CPU / latency
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When a pagination cursor points to transaction N within a ledger, `getTransactions` should jump directly to transaction N+1 rather than replaying and decoding the first N transactions again. Dense-ledger pagination should not scale with the number of skipped transactions.

## Mechanism

`processTransactionsInLedger` calls `reader.Seek(startTxIdx - 1)` before beginning the per-transaction loop. If `Seek` were implemented by repeatedly calling `Read()` from the start of the ledger, paginated requests into the middle of a dense ledger would waste CPU decoding transactions that are never returned.

## Trigger

1. Use a ledger with many transactions.
2. Call `getTransactions` with a cursor near the end of that ledger.
3. Compare CPU time against a version that jumps directly to the requested index.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:118-125` — the handler uses `reader.Seek(startTxIdx - 1)` to advance to the cursor.
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/ingest/ledger_transaction_reader.go:Seek:113-121` — the actual seek implementation.

## Evidence

The hot path looked suspicious because `Seek` happens before the transaction loop and there is no local proof that the reader can index directly into the ledger. Dense-ledger pagination is common enough that a linear seek would have been a meaningful source of wasted work.

## Anti-Evidence

The SDK implementation is explicit: `Seek` only bounds-checks the requested index and sets `reader.readIdx = index`. It does not replay prior transactions, so the cursor jump itself is O(1).

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-06
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The suspected replay cost does not exist. `Seek` is a direct index assignment, so pagination inside a ledger does not decode skipped transactions before returning the next page.

### Lesson Learned

When a hot path delegates cursor movement into an SDK helper, inspect the helper before assuming the skip is linear. The real per-ledger cost here is reader construction and envelope hashing, not the cursor jump itself.
