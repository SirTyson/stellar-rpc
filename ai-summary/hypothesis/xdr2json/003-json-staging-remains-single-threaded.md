# H003: `getTransactions` Still Prepares Every xdr2json Input on One Goroutine

**Date**: 2026-04-07
**Subsystem**: xdr2json
**Severity**: Medium
**Impact**: latency / CPU / multicore utilization
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once the relevant ledgers are already materialized in memory, the JSON path should
prepare per-transaction XDR inputs in parallel when those preparations are
independent. A large `getTransactions` page should not serialize all
`MarshalBinary()` and event-extraction work through one goroutine before the first
xdr2json batch even starts.

## Mechanism

Inside `processTransactionsInLedger()`, every JSON transaction immediately calls
`db.ParseTransaction()` inline. `ParseTransaction()` then marshals the result,
meta, and envelope, calls `GetTransactionEvents()`, and marshals every diagnostic,
transaction, and contract event into `[][]byte` form. Those per-transaction
operations are CPU-bound and independent once `reader.Read()` has returned the
`ingestTx`, but the current loop does all of them serially before appending to the
page-level `pending` batch.

## Trigger

1. Issue `getTransactions` with `format=json` on a page spanning 100-200
   transactions.
2. Use Soroban-heavy metas so `ParseTransaction()` has many event marshals.
3. Compare current behavior against a prototype that reads transactions in order
   but farms `ParseTransaction()` work to a bounded worker pool and then restores
   page order before batch conversion.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:117-205` — the per-ledger
  transaction loop performs JSON preparation inline at lines 152-163.
- `cmd/stellar-rpc/internal/db/transaction.go:276-315` — `ParseTransaction()`
  marshals `Result`, `Meta`, and `Envelope`, then extracts transaction events.
- `cmd/stellar-rpc/internal/db/transaction.go:319-354` — `parseEvents()` performs
  per-event `MarshalBinary()` work for all event families.

## Evidence

`processTransactionsInLedger()` already computes the cheap scalar response fields
(`hash`, `applicationOrder`, `feeBump`, ledger info, status) separately from the
JSON payload bytes, so the expensive staging work is isolated in `ParseTransaction()`.
After `reader.Read()` returns, that staging step no longer mutates shared ledger
state and only produces owned Go slices destined for the later batch conversion.

## Anti-Evidence

The reader itself is sequential, so only the post-`Read()` work can be parallelized.
If Rust-side conversion dominates a given workload, Go-side staging parallelism may
only move the overall needle by 5-20%, especially on small pages or ledgers with
few Soroban events.
