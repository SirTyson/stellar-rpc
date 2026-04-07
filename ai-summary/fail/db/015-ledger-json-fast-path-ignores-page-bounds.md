# H002: Ledger-level JSON extraction ignores cursor and limit bounds

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Medium
**Impact**: Rust JSON conversion / allocation overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

The raw-LCM JSON fast path should do work proportional to the transactions that can actually appear in the page. If the cursor starts in the middle of a ledger or the page fills after the first few transactions, the extractor should not serialize the rest of that ledger's transactions only to have Go discard them.

## Mechanism

`processChunksJSON()` computes `startTxIdx` and enforces `limit`, but it does so only after `LCMTransactionsToJSON(chunk.Lcm)` has already converted the entire ledger into per-transaction JSON. The Rust FFI surface accepts only the raw LCM blob, so `extract_transactions_json()` always loops every `tx_processing` entry in the ledger and serializes all of them before Go filters out `application_order < startTxIdx` and stops once `len(txns) >= limit`. On dense ledgers, that makes the new JSON fast path still O(transactions in ledger) instead of O(transactions returned in page).

## Trigger

Call `getTransactions` with `format=json` and either (a) a cursor in the middle of a dense ledger or (b) `limit=1` on a ledger containing many transactions. A profile should show Rust JSON conversion work for transactions whose `application_order` is below the cursor or above the final page boundary.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:processChunksJSON:281-299` — cursor start index is computed in Go, but not passed into the FFI call.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:processChunksJSON:328-335` — page limit is enforced only after the full ledger JSON result is already returned.
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:LCMTransactionsToJSON:42-78` — FFI wrapper accepts only raw LCM bytes, with no start/count bounds.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:lcm_transactions_to_json/extract_transactions_json:355-366,477-556` — Rust parses the LCM once and serializes every transaction in that ledger.

## Evidence

The FFI signature is `LCMTransactionsToJSON(lcmBytes []byte)` with no pagination parameters (`cmd/stellar-rpc/internal/xdr2json/conversion.go:45-59`). `processChunksJSON()` sets `startTxIdx` before the FFI call, but the filtering for `rtx.ApplicationOrder < startTxIdx` happens only inside the loop over already-materialized `rustTxns` (`cmd/stellar-rpc/internal/methods/get_transactions.go:281-301`). Rust-side extraction similarly computes `count = processing.len().min(tx_set.len())` and serializes each index from `0..count` without any page-bound pruning (`cmd/stellar-rpc/lib/xdr2json/src/lib.rs:505-556`).

## Anti-Evidence

This is most valuable when many transactions share a ledger; on sparse ledgers with one or two transactions each, whole-ledger extraction is already close to page-sized work. Any fix must still preserve the current cursor semantics and fee-bump/event grouping, so the FFI will need an application-order-aware extraction contract rather than a simple slice truncation.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — distinct mechanism from fail/db/012 (FFI-level bounds vs. planner query), though same dense-ledger precondition
**Failed At**: reviewer

### Trace Summary

Traced the full `processChunksJSON` path through the Rust FFI (`lcm_transactions_to_json` → `extract_transactions_json`). Confirmed the code pattern exists: Rust serializes all transactions in the LCM to JSON before Go filters by `startTxIdx` and `limit`. However, the upstream planner (`GetLedgerSequencesWithTransactions`, lines 400-411) already uses row-level precision to fetch only the minimal set of ledger sequences, and real futurenet workloads have ~1 transaction per ledger — making the within-ledger waste effectively zero.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:processChunksJSON:257-340` — FFI call at line 288 converts entire LCM; Go filtering at lines 298-332 discards extras
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:LCMTransactionsToJSON:45-79` — FFI signature takes only `lcmBytes []byte`, no bounds parameters
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:extract_transactions_json:477-557` — Rust loop `for i in 0..count` serializes every transaction with `serde_json::to_value` for result, meta, envelope, and events
- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:400-411` — existing row-level planner already limits which ledgers are fetched to those containing page-relevant transactions

### Why It Failed

The inefficiency is real in theory but does not manifest on actual workloads. The existing index-driven planner (`GetLedgerSequencesWithTransactions`) already ensures only ledgers containing page-relevant transactions are fetched. The within-ledger waste (converting transactions before cursor or after limit) only matters with dense ledgers (many txns per ledger). Real futurenet benchmark data shows `avg_tx_per_ledger ≈ 1.00` and `max_tx_per_ledger = 2` (established during fail/db/012's final review benchmarks at 300/500/700 RPS), meaning each LCM contains at most 1-2 transactions — the FFI converts exactly what the page needs. Adding `start_idx`/`count` parameters to the FFI interface would increase CGo/Rust interface complexity and add branching inside the Rust extraction loop for zero measurable benefit.

### Lesson Learned

Dense-ledger optimizations (reducing per-ledger transaction processing waste) share the same fundamental limitation: real Stellar network workloads produce sparse ledgers (~1 tx/ledger). Both this hypothesis and fail/db/012 target the same unrealized precondition. Future hypotheses should verify ledger density on the actual benchmark workload before proposing within-ledger optimizations.
