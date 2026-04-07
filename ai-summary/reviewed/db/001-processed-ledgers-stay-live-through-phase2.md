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

---

## Review

**Verdict**: VIABLE
**Severity**: Informational
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated (H009 covers by-value copy overhead, not heap retention of processed entries)

### Trace Summary

Traced the full `getTransactions` two-phase flow. Phase 1 (`fetchLedgerMetas`) accumulates `[]xdr.LedgerCloseMeta` in batches of 50 until `totalTxCount >= limit` (max 200). Phase 2 iterates with `for _, ledger := range collectedMetas`, copying the 32-byte struct value into `ledger` but leaving the original slice entry (and its pointer to `*LedgerCloseMetaV2`) intact. Confirmed that `LedgerCloseMeta` is a union with pointer fields (`V0/V1/V2 *LedgerCloseMetaVN`), so the slice entries keep the full deserialized object graphs reachable through the entire phase-2 loop.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:278-307` — phase-2 loop uses `for _, ledger := range collectedMetas` without clearing processed entries
- `cmd/stellar-rpc/internal/methods/get_transactions.go:fetchLedgerMetas:312-407` — accumulates LCMs in batches of 50, stops when `totalTxCount >= limit`
- `cmd/stellar-rpc/internal/db/ledger.go:BatchGetLedgerMetas:133-155` — returns `[]xdr.LedgerCloseMeta` fully deserialized from SQLite
- `xdr_generated.go:LedgerCloseMeta:20853-20858` — confirmed struct is `{V int32, V0 *V0, V1 *V1, V2 *V2}` (32 bytes with pointer fields)
- `xdr_generated.go:LedgerCloseMetaV2:20599-20608` — pointed-to struct contains `TxSet`, `TxProcessing []TransactionResultMetaV1`, etc.
- `cmd/stellar-rpc/internal/config/options.go:362` — max limit = 200, default = 50

### Findings

The inefficiency is **real**: processed `LedgerCloseMeta` entries remain GC-reachable through the `collectedMetas` slice for the duration of phase 2. The fix (clearing `collectedMetas[i] = xdr.LedgerCloseMeta{}` after processing each entry) is **correct** and **safe** — no downstream code re-accesses processed entries, and `processTransactionsInLedger` receives the value by copy.

However, the **practical impact is bounded** by several factors:

1. **Limit cap**: The max limit of 200 transactions bounds how many ledgers can be retained. The relationship between ledger count and per-ledger size is inverse — sparse histories yield many small LCMs, dense histories yield few large LCMs — so total retained bytes stay modest regardless of workload.
2. **Sparse case**: limit=200 with 1 tx/ledger → 200 retained LCMs, each containing minimal `TxProcessing` data (few KB each) → total ~200KB-1MB of stale heap. Negligible GC impact.
3. **Dense case**: limit=200 with 50 tx/ledger → 4 retained LCMs, each potentially large (several MB) but only 3 stale entries → a few MB of stale heap. Still small relative to the response being built and total server heap.
4. **Short-lived**: The stale references exist only for the duration of phase-2 processing of a single request, not across requests.

Downgraded from **Medium** to **Informational** because the retained data is unlikely to produce measurable latency or throughput effects within the 200-transaction limit. The fix is a best-practice improvement (clearing dead references to allow earlier GC collection) rather than a performance-critical optimization.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/methods/get_transactions.go:289-297` — change the phase-2 loop from `for _, ledger := range collectedMetas` to an indexed loop that clears each slot after processing:
  ```go
  for i := range collectedMetas {
      ledger := collectedMetas[i]
      collectedMetas[i] = xdr.LedgerCloseMeta{} // allow GC of processed LCM
      cursor, done, err = h.processTransactionsInLedger(ctx, ledger, start, &txns, limit, request.Format, enc)
      // ... rest unchanged
  }
  ```
- **Correctness check**: existing `TestGetTransactions_*` tests in `get_transactions_test.go` cover the loop behavior
- **Benchmark focus**: heap profile (`runtime.MemStats.HeapInuse`) during large sparse getTransactions requests. Expect a small reduction in peak heap but likely below noise for latency/RPS metrics
