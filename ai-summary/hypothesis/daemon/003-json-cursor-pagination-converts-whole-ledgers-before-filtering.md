# H003: JSON Cursor Pagination Converts Whole Ledgers Before Filtering

**Date**: 2026-04-07
**Subsystem**: daemon
**Severity**: High
**Impact**: wasted Rust parse/JSON serialization on dense-ledger pages
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When a JSON `getTransactions` request starts partway through a dense ledger or asks for only a small page, the daemon should convert only the transactions needed for that page. A cursor that starts near transaction 180 of a 200-transaction ledger should not force JSON extraction for transactions 1-179 before discarding them.

## Mechanism

`processChunksJSON` computes `startTxIdx` before calling into Rust, but it still invokes `xdr2json.LCMTransactionsToJSON(chunk.Lcm)` on the full ledger and applies both the cursor filter and the page limit only after Rust has already parsed and JSON-serialized every transaction in that ledger. The Rust helper `extract_transactions_json` similarly walks `0..count` and builds a JSON object for every transaction before returning the full array to Go. On repeated cursor pagination through a dense ledger, this turns later pages into O(all-transactions-in-ledger) work instead of O(transactions-returned).

## Trigger

Use `getTransactions` with `format=json`, a cursor inside a dense ledger, and a small `limit` such as `10` or `25`. Compare the current implementation to a version that passes `(startTxIdx, remainingLimit)` into the Rust FFI and extracts only the needed application-order range from the ledger.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:277-299` — computes `startTxIdx`, then calls `LCMTransactionsToJSON`, then filters `rtx.ApplicationOrder < startTxIdx` in Go.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:324-331` — page `limit` is enforced only after full-ledger Rust conversion has completed.
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:42-78` — Go FFI wrapper exposes only a whole-ledger `LCMTransactionsToJSON` API.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:356-381` — Rust FFI parses the full `LedgerCloseMeta` and returns one JSON payload for the whole ledger.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:475-556` — `extract_transactions_json` iterates every transaction and serializes every result into the response array.

## Evidence

The current code records the cursor boundary before entering Rust, but the only filtering happens after `rustTxns` has already been materialized in Go. The Rust extraction loop has no notion of cursor or page limit, so it necessarily serializes every transaction and event in the ledger even when the handler will return only the tail of that ledger.

## Anti-Evidence

The win is workload-sensitive: first-page requests starting at transaction 1 or requests that consume most of a ledger will not recover much work. The FFI change also requires a shared API update between Go and Rust, so the optimization is more invasive than a pure Go cache tweak.
