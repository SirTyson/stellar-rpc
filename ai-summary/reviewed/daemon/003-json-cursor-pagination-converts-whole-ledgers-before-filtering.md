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

---

## Review

**Verdict**: VIABLE
**Severity**: Medium
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full `getTransactions` JSON path from `getTransactionsByLedgerSequence` through `processChunksJSON` into the Rust FFI `lcm_transactions_to_json` → `extract_transactions_json`. Confirmed that the SQL index query (`GetLedgerSequencesWithTransactions`) already uses row-level precision to minimize the number of fetched ledgers, but the Rust FFI operates on whole ledgers with no cursor or limit awareness. The Go-side filtering at lines 294-298 and 324-328 discards already-serialized JSON objects, which is pure wasted work.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:253-336` — `processChunksJSON` calls `LCMTransactionsToJSON` on the full ledger bytes, then iterates over the returned array, skipping `rtx.ApplicationOrder < startTxIdx` and breaking at `limit`. All discarded items were fully JSON-serialized by Rust.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:396-407` — `GetLedgerSequencesWithTransactions` uses row-level `LIMIT` on `(ledger_sequence, application_order)` pairs, so for `limit=10` on a 200-tx ledger, only 1 ledger is fetched — but its entire content is serialized.
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:42-79` — `LCMTransactionsToJSON` passes the raw LCM bytes to Rust with no range parameters, receives the full JSON array, and unmarshals it into `[]LCMTransactionJSON`.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:356-382` — `lcm_transactions_to_json` parses the entire `LedgerCloseMeta` and delegates to `extract_transactions_json`.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:477-557` — `extract_transactions_json` iterates `0..count` unconditionally, calling `serde_json::to_value` on every `TransactionResult`, `TransactionMeta`, `TransactionEnvelope`, and event structure for every transaction in the ledger.
- `cmd/stellar-rpc/internal/db/transaction.go:171-201` — SQL index query already has row-level precision, making the over-serialization the sole remaining waste.

### Findings

The inefficiency is confirmed and real. Two forms of waste exist:

1. **Cursor waste (first ledger)**: On continuation pages where the cursor starts mid-ledger (e.g., tx 180 of 200), Rust serializes all 200 transactions but Go discards the first 179. This is 179 wasted `serde_json::to_value` calls on heavy structures (TransactionMeta, events, etc.).

2. **Limit waste (any ledger)**: When the page limit is reached mid-ledger (e.g., `limit=10` on a 200-tx ledger), Rust has already serialized all 200 transactions but Go uses only 10. This is 190 wasted serializations.

The SQL index query's row-level LIMIT actually makes the waste MORE concentrated: for `limit=10` on a dense 200-tx ledger, only 1 LCM is fetched but 95% of the Rust serialization work is discarded.

**Severity downgraded to Medium**: The waste is workload-dependent. First-page requests (most common) typically start at `startTxIdx=1`, so cursor waste is zero for those. The limit waste is significant for small-page requests on dense ledgers but less impactful for large-page requests. For `limit=10` on a 200-tx ledger, Rust does ~20x more serialization than needed, which could represent a 5-15% total request latency reduction for that pattern. Across mixed traffic patterns, the aggregate improvement is likely in the 5-20% range for JSON format, which fits Medium severity.

### PoC Guidance

- **Target code**: Modify `cmd/stellar-rpc/lib/xdr2json/src/lib.rs` to add a new FFI function `lcm_transactions_to_json_range(lcm_bytes, start_idx, max_count)` that passes range parameters into `extract_transactions_json`. In `extract_transactions_json`, skip serialization for indices < `start_idx` and stop after `max_count` items. Update `cmd/stellar-rpc/internal/xdr2json/conversion.go` to expose a `LCMTransactionsToJSONRange(lcmBytes, startIdx, limit)` wrapper. Update `processChunksJSON` to pass `startTxIdx` and remaining limit to the new function.
- **Change description**: Add `start_idx` (0-based or 1-based matching existing convention) and `max_count` parameters to the Rust extraction function. In the `for i in 0..count` loop, `continue` for `i < start_idx` and `break` after emitting `max_count` items. The XDR parse of the LCM itself is unavoidable (Rust needs the full LCM to access tx_processing[i]), but the expensive `serde_json::to_value` calls can be skipped for out-of-range transactions.
- **Correctness check**: Existing `getTransactions` tests with JSON format should pass unchanged. The `application_order` field in returned transactions must still be correct (it's derived from the index, not the iteration count, so skipping items doesn't affect it).
- **Benchmark focus**: Measure `getTransactions` JSON latency with `limit=10` and a cursor mid-way through a 100+ tx ledger. The Rust serialization time for the single ledger should drop proportionally to (limit / total_txns_in_ledger). Expect ~5-15% total request latency improvement for small-page pagination on dense ledgers.
