# H001: Processed ledgers stay live for the full request after phase-1 prefetch

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Medium
**Impact**: heap retention / GC pressure
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once `getTransactions` has finished processing one fetched ledger, that ledger's `LedgerCloseMeta` object graph should no longer stay reachable from long-lived request state. Peak live heap should track the current ledger batch and the response being built, not every prefetched ledger for the entire request lifetime.

## Mechanism

`fetchLedgerMetas()` materializes all prefetched `xdr.LedgerCloseMeta` values into `collectedMetas`, and phase 2 then iterates them with `for _, ledger := range collectedMetas`. Because the original slice entries are never cleared, each processed `LedgerCloseMeta` remains a GC root until the handler returns, even though phase 2 no longer needs it after `processTransactionsInLedger()` finishes. On large JSON pages or sparse scans that fetch many ledgers, this keeps already-consumed XDR trees live alongside the growing `[]protocol.TransactionInfo`, increasing GC assist work and peak heap.

## Trigger

Request `getTransactions` with `format=json` and a large limit over dense or sparse history so `fetchLedgerMetas()` collects many ledgers before phase 2 starts. Heap profiles taken during the request should show multiple already-processed `LedgerCloseMeta` trees still retained from `collectedMetas` while later ledgers are being serialized.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:278-297` — phase-2 loop processes ledgers but never clears processed slots
- `cmd/stellar-rpc/internal/methods/get_transactions.go:fetchLedgerMetas:312-407` — phase 1 accumulates all fetched `xdr.LedgerCloseMeta` values into one long-lived slice

## Evidence

`fetchLedgerMetas()` appends every fetched ledger into `collectedMetas` and returns that slice to phase 2 (`cmd/stellar-rpc/internal/methods/get_transactions.go:352-407`). Phase 2 ranges over the slice by value (`cmd/stellar-rpc/internal/methods/get_transactions.go:289-297`), so the original slice entries continue holding the full `LedgerCloseMeta` pointer graph after each ledger is done. `xdr.LedgerCloseMeta` is a small union header that points at large per-ledger trees, which means the retained cost is in the referenced XDR data, not in the 32-byte slice element itself.

## Anti-Evidence

Small requests that finish in one or two ledgers will see little benefit because there are few stale entries to retain. The larger reviewed optimizations that reduce how many ledgers are fetched at all (index-driven planning and lazy decode) would shrink this problem, so the win here is bounded and workload-dependent.
