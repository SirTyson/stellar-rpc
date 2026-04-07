# H004: Rust JSON Output Is Immediately Reparsed in Go

**Date**: 2026-04-07
**Subsystem**: daemon
**Severity**: Medium
**Impact**: redundant JSON encode/decode across the Rust-Go boundary
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once the Rust `xdr2json` path has already produced compact per-transaction JSON for a `getTransactions` page, the daemon should carry that JSON forward without immediately tokenizing it back into Go structs. The FFI boundary should not force a large JSON string to be serialized in Rust and then fully parsed again in Go before the response is written.

## Mechanism

`lcm_transactions_to_json` builds a `Vec<serde_json::Value>` and serializes the entire ledger's transaction array to a JSON string in Rust. Go then copies that byte buffer with `C.GoBytes` and immediately calls `json.Unmarshal` into `[]LCMTransactionJSON`, only to copy those `json.RawMessage` fields into `protocol.TransactionInfo` for later response serialization. Returning structured FFI results (or raw per-transaction JSON fragments plus scalar sidecar fields) would remove the large Rust string build plus the matching Go JSON parse from every JSON-mode page.

## Trigger

Benchmark `getTransactions` with `format=json` on large pages (`limit=50` or `200`) and compare the current Rust-stringify/Go-unmarshal path to a version where the FFI returns per-transaction JSON fragments and scalar metadata directly, so Go can populate the response without `json.Unmarshal`.

## Target Code

- `cmd/stellar-rpc/internal/xdr2json/conversion.go:45-78` — `LCMTransactionsToJSON` copies Rust's JSON string into Go and then `json.Unmarshal`s it into `[]LCMTransactionJSON`.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:283-324` — handler copies the parsed Rust payload into `protocol.TransactionInfo` structs field-by-field.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:356-381` — `lcm_transactions_to_json` returns one JSON string buffer for the whole ledger.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:505-556` — Rust builds a `Vec<serde_json::Value>` and serializes the full array with `serde_json::to_string`.

## Evidence

The Rust helper already has all scalar metadata and all per-transaction JSON fields in hand before it serializes the result array to a string. The Go wrapper then immediately pays `json.Unmarshal` on that same payload, which means the optimized raw-LCM path still includes a full JSON encode/decode round-trip between Rust and Go before any HTTP serialization even begins.

## Anti-Evidence

This change is more invasive than a local Go optimization because it alters the FFI contract, and some later response serialization work remains unless the daemon also adopts a raw-JSON response encoder. If JSON-mode traffic is rare compared to default XDR-mode traffic, overall endpoint impact may stay moderate.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated. Distinct from fail/daemon/009 (which targeted the OLD `processTransactionsInLedger` path) and from fail/xdr2json/007 (which targeted `ConvertBytes` C string bridge). This hypothesis targets the ACTIVE `processChunksJSON` / `LCMTransactionsToJSON` code path.

### Trace Summary

Traced the full `processChunksJSON` pipeline from DB fetch through Rust FFI to Go struct population. Confirmed the active JSON code path (get_transactions.go:438-471) calls `LCMTransactionsToJSON`, which invokes Rust's `lcm_transactions_to_json`. Rust deserializes the full LCM via `read_xdr_to_end`, then builds a `Vec<serde_json::Value>` JSON DOM per transaction (with `serde_json::to_value` for each field), wraps them in a `json!({})` macro, aggregates into a `Vec`, and serializes the entire array to a single JSON string via `serde_json::to_string`. Go then copies this string via `C.GoBytes` and scans the entire payload with `json.Unmarshal` to extract fields — the large `json.RawMessage` fields (meta, result, envelope, events) are bracket-counted and byte-copied rather than deeply parsed, but the full scan is still O(total output size).

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:438-471` — JSON format branch calls `BatchGetLedgersBySequences`, releases DB snapshot, then calls `processChunksJSON`
- `cmd/stellar-rpc/internal/methods/get_transactions.go:257-340` — `processChunksJSON` iterates chunks, calls `xdr2json.LCMTransactionsToJSON(chunk.Lcm)` per ledger, copies scalar + RawMessage fields into `protocol.TransactionInfo`
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:45-79` — `LCMTransactionsToJSON` pins Go bytes, calls C `lcm_transactions_to_json`, copies result with `C.GoBytes`, then `json.Unmarshal` into `[]LCMTransactionJSON`
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:356-382` — `lcm_transactions_to_json` calls `read_xdr_to_end` on full LCM, then `extract_transactions_json`, converts result string to boxed bytes for FFI return
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:477-557` — `extract_transactions_json` builds `Vec<serde_json::Value>` with `serde_json::to_value()` per field (lines 526-531), `extract_events` (lines 444-468) also uses `to_value` per event, and final `serde_json::to_string(&result_array)` serializes the whole DOM tree to one string (line 556)

### Findings

**The inefficiency is real and targets the active code path.** The `processChunksJSON` path (used for all `format=json` requests not served from cache, which explicitly excludes JSON per line 418) exercises this pattern for every page.

**Three distinct sources of waste identified:**

| Step | Operation | Waste |
|------|-----------|-------|
| Rust: `serde_json::to_value()` per field | Builds JSON DOM tree (many small heap allocations for Value nodes) | Eliminable — `serde_json::to_vec()` serializes directly to bytes without intermediate DOM |
| Rust: `serde_json::to_string(&result_array)` | Full tree traversal to serialize DOM to string | Eliminable — with per-field byte buffers, no array-level serialization needed |
| Go: `json.Unmarshal(jsonBytes, &txns)` | O(n) tokenization/bracket-scan of entire payload | Eliminable — with structured FFI returns, Go receives fields directly |

**A structured FFI return would replace all three steps** with per-field `serde_json::to_vec()` calls (same serialization work but without DOM allocation) plus per-field `C.GoBytes` copies (same total byte volume). Net savings: eliminate the JSON DOM heap allocations, the array-level serialization pass, and the Go-side JSON scan.

**Severity downgrade rationale:** The hypothesis claims Medium (5-20%). While the combined waste (DOM construction + array serialization + Go unmarshal) is more substantial than the per-call savings targeted by fail/xdr2json/007 (C string bridge, which was benchmarked and showed no consistent endpoint improvement), it remains secondary to the dominant costs of XDR deserialization (`read_xdr_to_end` of multi-MB LCMs) and per-field JSON serialization (which remains unchanged). The JSON DOM allocation cost is the most significant component of the waste — for large TransactionMeta with many state diffs/events, `to_value` creates hundreds of small heap allocations that `to_vec` would avoid — but this is still estimated at <5% of total per-page processing time. Low severity is appropriate.

**No correctness concerns.** The `LCMTransactionJSON` struct fields are all directly assignable from per-field byte buffers (`json.RawMessage` is `[]byte`). Scalars (hash, application_order, fee_bump, successful) are already extracted as typed values in Rust. The Go-side field assignment in `processChunksJSON` (lines 305-327) would simplify rather than complicate.

### PoC Guidance

- **Target code**:
  - `cmd/stellar-rpc/lib/xdr2json/src/lib.rs`: Create a new FFI return type (e.g., `LcmTransactionResult`) with per-transaction fields: `hash_ptr/hash_len`, `application_order` (i32), `fee_bump` (bool), `successful` (bool), and per-field JSON byte buffers (`result_ptr/result_len`, `meta_ptr/meta_len`, `envelope_ptr/envelope_len`, `diag_events_ptr/diag_events_len`, etc.). In `extract_transactions_json`, replace `serde_json::to_value()` with `serde_json::to_vec()` for each field, and return the array of structured results instead of a single JSON string. Add a corresponding `free_lcm_transactions_result` function.
  - `cmd/stellar-rpc/lib/xdr2json.h`: Add the new C struct definition for per-transaction results.
  - `cmd/stellar-rpc/internal/xdr2json/conversion.go`: Replace `json.Unmarshal` in `LCMTransactionsToJSON` with direct extraction from the structured FFI return — `C.GoBytes` per JSON field, direct assignment for scalars.
- **Change description**: Replace the single-JSON-string FFI contract in `lcm_transactions_to_json` with a structured per-transaction return. Rust serializes each field directly to bytes via `serde_json::to_vec()` (eliminating the intermediate `serde_json::Value` DOM) and returns scalars + per-field byte buffers. Go extracts them without `json.Unmarshal`.
- **Correctness check**: Existing Go tests in `cmd/stellar-rpc/internal/methods/get_transactions_test.go` and integration tests exercise `processChunksJSON` through JSON-format requests. Rust tests for `lcm_transactions_to_json` (if any) also apply. All should pass unchanged since output bytes are identical.
- **Benchmark focus**: Measure `getTransactions` with `format=json`, `limit=200` on Soroban-heavy ledgers. Primary metric: p50/p95 latency at the 150-175 RPS range. Secondary: heap allocation count and bytes per page (expect reduction from eliminating JSON DOM nodes). Expect <5% latency improvement — if the DOM allocation savings are larger than anticipated, could reach 5%.
