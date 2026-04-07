# H006: Empty-ledger scans build a transaction reader before checking the count

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Low
**Impact**: CPU / allocation overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When `getTransactions` encounters an empty ledger, it should skip that ledger with minimal additional setup. Ideally it would check the transaction count first and avoid constructing any per-ledger reader state when there is nothing to read.

## Mechanism

I suspected the call order in `processTransactionsInLedger()` was wasting work on empty ledgers because it creates `ingest.NewLedgerTransactionReaderFromLedgerCloseMeta()` before it reads `ledger.CountTransactions()`. If reader construction were expensive even for zero-transaction ledgers, sparse or no-result scans would pay that penalty for every empty ledger touched.

## Trigger

Call `getTransactions` across a run of empty ledgers so the handler repeatedly enters `processTransactionsInLedger()` and immediately discovers there are no transactions to return.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:86-110` — reader is created before `txCount := ledger.CountTransactions()`
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/ingest/ledger_transaction_reader.go:48-61,125-149` — constructor and `storeTransactions`
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/xdr/ledger_close_meta.go:49-60` — `CountTransactions()` is a cheap length lookup on the already-decoded LCM

## Evidence

The current code definitely constructs the reader before it checks `ledger.CountTransactions()` (`cmd/stellar-rpc/internal/methods/get_transactions.go:86-110`), so there is an avoidable ordering issue on paper. `CountTransactions()` itself is just a `len(...)` over the relevant `TxProcessing` slice (`/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/xdr/ledger_close_meta.go:49-60`).

## Anti-Evidence

The constructor's heavy path only shows up when the ledger actually has envelopes to hash. On an empty ledger, `NewLedgerTransactionReaderFromLedgerCloseMeta()` allocates a zero-sized map and `storeTransactions()` iterates over zero envelopes, so the incremental cost is tiny compared with the already-paid ledger blob read and LCM unmarshal. The meaningful sparse-history optimization is to stop fetching empty ledgers in the first place, which is already captured by the reviewed index-driven scan hypothesis.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

Reordering the count check would remove only a tiny constant on empty ledgers because the expensive hashing/setup path in the SDK reader does not run when `CountTransactions()==0`.

### Lesson Learned

For sparse `getTransactions` workloads, the important waste is at the ledger-fetch level, not at the zero-transaction reader-construction level. Micro-optimizations after the full LCM has already been fetched and unmarshaled are usually too small unless they eliminate whole-ledger work.
