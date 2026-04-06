# H006: Exhausted Cursor Ledgers Still Build a Full Transaction Reader Before Returning Nothing

**Date**: 2026-04-06
**Subsystem**: methods
**Severity**: Medium
**Impact**: latency / CPU
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When a pagination cursor already points past the last transaction in the current ledger, `getTransactions` should skip that ledger without constructing any `ingest.LedgerTransactionReader`. The next page after a ledger-boundary cursor should advance directly to the following ledger and spend essentially zero CPU on the exhausted ledger.

## Mechanism

`processTransactionsInLedger` constructs the SDK reader before it computes `txCount` or checks whether `start.TransactionOrder` is already greater than the ledger's transaction count. On the common boundary case where the previous page ended on the last transaction of a dense ledger, the next request increments the cursor to `txCount + 1`, the loop body never runs, but `NewLedgerTransactionReaderFromLedgerCloseMeta` has already hashed every envelope in the ledger and built its hash map. That turns a zero-result ledger skip into O(txCount) CPU work.

## Trigger

1. Request a page whose cursor lands on the final transaction of a ledger with many transactions.
2. Issue the next `getTransactions` request with that cursor.
3. Compare CPU/allocation profiles before and after adding a fast path that checks `startTxIdx > txCount` before constructing the reader.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:94-130` — reader construction happens before the handler knows whether the starting transaction index is already past the ledger.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:118-125` — `Seek` returning `io.EOF` is tolerated, but only after the expensive reader was built.
- `github.com/stellar/go-stellar-sdk/ingest/ledger_transaction_reader.go:48-60` — reader construction eagerly populates per-ledger state.
- `github.com/stellar/go-stellar-sdk/ingest/ledger_transaction_reader.go:125-149` — `storeTransactions` hashes every envelope in the ledger before the first `Read`.

## Evidence

The handler computes `startTxIdx` from the cursor only after reader creation, and the SDK reader constructor immediately executes `storeTransactions`. If `startTxIdx` is `txCount + 1`, the `for i := startTxIdx; i <= txCount; i++` loop is skipped entirely, so all of that hashing and map population was wasted. This is materially different from the already-rejected empty-ledger case because the wasted setup cost scales with the number of transactions in the just-finished ledger.

## Anti-Evidence

The waste only occurs when a page boundary lands exactly at the end of a ledger; requests that start in the middle of a ledger still need the reader. Sparse scans dominated by DB fetch cost will see less benefit than dense sequential pagination.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-06
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full code path from `processTransactionsInLedger` (get_transactions.go:104) through `NewLedgerTransactionReaderFromLedgerCloseMeta` (ledger_transaction_reader.go:48-62) into `storeTransactions` (lines 125-149). Confirmed that reader construction eagerly iterates every envelope via `TransactionEnvelopes()`, XDR-marshals each one into a `TransactionSignaturePayload`, SHA-256 hashes it via `network.HashTransactionInEnvelope`, and inserts into a pre-allocated map. For the exhausted-cursor case, this entire O(txCount) chain executes but zero transactions are subsequently read. The cost per envelope is ~2-6 µs (XDR marshal + SHA-256 + map insert), giving ~200-600 µs wasted for a 100-tx ledger. This is comparable to the SQL fetch cost for that ledger (~100-1000 µs) but represents <5% of total paginated request latency since it affects only the first ledger per request.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:104` — calls `NewLedgerTransactionReaderFromLedgerCloseMeta` before computing `startTxIdx` or `txCount`
- `cmd/stellar-rpc/internal/methods/get_transactions.go:112-126` — `startTxIdx` is set from cursor only after reader is built; `Seek` returning `io.EOF` is tolerated but reader already paid full setup cost
- `cmd/stellar-rpc/internal/methods/get_transactions.go:128-130` — `txCount` computed after reader construction; loop `for i := startTxIdx; i <= txCount` never executes when `startTxIdx > txCount`
- `go-stellar-sdk@v0.4.0/ingest/ledger_transaction_reader.go:48-62` — constructor allocates map with `capacity=CountTransactions()` and immediately calls `storeTransactions`
- `go-stellar-sdk@v0.4.0/ingest/ledger_transaction_reader.go:125-149` — `storeTransactions` iterates all envelopes, XDR-marshals each into a `TransactionSignaturePayload`, hashes via SHA-256
- `go-stellar-sdk@v0.4.0/network/main.go:hashTx` — allocates `bytes.Buffer`, XDR-marshals full `TransactionSignaturePayload`, calls `hash.Hash` (SHA-256)
- `go-stellar-sdk@v0.4.0/xdr/ledger_close_meta.go:62-93` — `TransactionEnvelopes()` flattens phases/components/clusters into a new slice (allocation + iteration)
- `cmd/stellar-rpc/internal/methods/get_transactions.go:40-68` — `initializePagination` increments `start.TransactionOrder` past the last tx when cursor lands on ledger boundary

### Findings

The inefficiency is real: when a pagination cursor points past the last transaction in a ledger, `processTransactionsInLedger` constructs a full `LedgerTransactionReader` at O(txCount) cost (XDR serialization + SHA-256 per envelope + map allocation), then reads zero transactions. The fix is to move the `txCount` check before reader construction.

This is distinct from H003 (empty-ledger case, where cost is negligible because `make(map, 0)` is free and the envelope loop is empty) and H010 (removing the hash map entirely, which breaks correctness). H006 targets a specific pagination boundary case with non-empty ledgers where the waste scales linearly with transaction count.

The per-request impact is ~200-600 µs for a 100-tx ledger, which is typically 1-5% of total paginated request latency (~5-20 ms). Downgraded from Medium to Low severity since the 5-20% threshold for Medium requires the exhausted ledger to contain ~300+ transactions, which is uncommon on mainnet. The fix is trivially correct and carries zero correctness risk.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/methods/get_transactions.go`, function `processTransactionsInLedger` (lines 94-213)
- **Change description**: Move `txCount := ledger.CountTransactions()` and `startTxIdx` computation above the `NewLedgerTransactionReaderFromLedgerCloseMeta` call. Add early return `if startTxIdx > txCount { return toid.New(ledgerSeqInt32, 0, 1), false, nil }` before reader construction. The `ledgerSeqInt32` computation must also move up since it's needed for the early return.
- **Correctness check**: Existing tests in `get_transactions_test.go` cover pagination scenarios. Run `go test ./cmd/stellar-rpc/internal/methods/...` to verify no regressions. Key test cases: pagination across ledger boundaries, cursor pointing to last tx in a ledger.
- **Benchmark focus**: Benchmark `processTransactionsInLedger` with a cursor pointing past a 100-tx ledger. Expect ~200-600 µs reduction per call in this scenario. Allocations should drop by ~txCount map entries + txCount XDR buffer allocations.
