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
