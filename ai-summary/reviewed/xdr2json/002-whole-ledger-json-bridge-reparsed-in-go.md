# H002: `LCMTransactionsToJSON` Still Round-Trips Through a Whole-Ledger JSON Document in Go

**Date**: 2026-04-07
**Subsystem**: xdr2json
**Severity**: Medium
**Impact**: latency / CPU / allocations / GC
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once Rust has extracted per-transaction JSON fragments from a raw
`LedgerCloseMeta`, Go should receive those fragments in a structured FFI result
that it can assign directly into `protocol.TransactionInfo`. The hot path should
not serialize a temporary ledger-wide JSON array and then immediately parse that
document back into Go structs before the final RPC response is marshaled.

## Mechanism

`lcm_transactions_to_json()` produces one large JSON string for the entire ledger,
and `LCMTransactionsToJSON()` copies that buffer into Go and runs
`json.Unmarshal(jsonBytes, &txns)`. That outer array/object serialization is
staging-only work: Go ultimately re-serializes the response later, and the helper
only needs the raw per-field fragments plus a few scalars. Replacing the
whole-document bridge with an FFI result shaped like `[]LCMTransactionJSON` (or
parallel fragment buffers plus scalar metadata) would remove one full JSON encode
of the ledger container and one full JSON decode of that container in Go.

## Trigger

1. Issue `getTransactions` with `format=json` for 100-200 transactions with large
   `resultMetaJson` and event payloads.
2. Profile CPU and allocations around `lcm_transactions_to_json`,
   `C.GoBytes`, and `encoding/json.Unmarshal`.
3. Compare against a prototype that returns per-transaction structured results
   directly over FFI instead of a ledger-wide JSON array string.

## Target Code

- `cmd/stellar-rpc/internal/xdr2json/conversion.go:45-79` — Go copies the helper output and reparses it with `json.Unmarshal`.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:351-377` — `lcm_transactions_to_json()` returns a single `ConversionResult` containing one JSON blob.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:475-556` — Rust serializes the ledger into one array string via `serde_json::to_string(&result_array)`.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:281-323` — the parsed Go structs are then rewrapped into `protocol.TransactionInfo`.

## Evidence

The Go wrapper does `jsonBytes := C.GoBytes(...)` and then `json.Unmarshal(...)`
for every selected ledger. The Rust side serializes `"hash"`, `"application_order"`,
`"result"`, `"meta"`, `"envelope"`, and event arrays into a temporary outer JSON
document even though Go only needs to preserve those fragments for the final
response shape.

## Anti-Evidence

`json.RawMessage` means Go does not deeply decode the nested `result`, `meta`,
`envelope`, and event bodies, so this is not a full second parse of every inner
JSON fragment. The endpoint still needs at least one owned copy into Go-managed
memory before the response escapes cgo.

---

## Review

**Verdict**: VIABLE
**Severity**: Medium
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated. Fail/009 addressed the old Go XDR decode→encode→decode pipeline (before `processChunksJSON` existed); fail/007 addressed the per-call C-string bridge; fail/011 addressed `to_string` vs `to_vec` API surface. None target the `extract_transactions_json` Value-tree intermediate or the Go-side `json.Unmarshal` within the current `processChunksJSON` path.

### Trace Summary

Traced the complete `processChunksJSON` → `LCMTransactionsToJSON` → Rust `lcm_transactions_to_json` → `extract_transactions_json` pipeline. Confirmed two distinct inefficiencies: (1) Rust builds `serde_json::Value` trees for every component (`to_value(&result_pair.result)`, `to_value(meta)`, `to_value(envelope)`, `to_value(event)`) and then serializes the entire Value tree to a JSON string via `to_string(&result_array)` — a two-pass serialization where each byte is processed twice; (2) Go copies this JSON string via `C.GoBytes` and then scans the entire payload via `json.Unmarshal` to extract per-transaction fragments as `json.RawMessage`, which makes separate byte-slice copies for each fragment rather than receiving pre-separated buffers.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:438-447` — FormatJSON path calls `processChunksJSON`, completely bypassing `processTransactionsInLedger`; confirms this is the active JSON code path
- `cmd/stellar-rpc/internal/methods/get_transactions.go:282` — `xdr2json.LCMTransactionsToJSON(chunk.Lcm)` invoked once per ledger with raw LCM bytes from SQLite
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:45-79` — `LCMTransactionsToJSON`: pins Go bytes, passes to Rust via `C.lcm_transactions_to_json`, copies result via `C.GoBytes`, runs `json.Unmarshal(jsonBytes, &txns)` into `[]LCMTransactionJSON`
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:29-40` — `LCMTransactionJSON` struct: scalar fields (Hash, ApplicationOrder, FeeBump, Successful) plus `json.RawMessage` fields (ResultJSON, MetaJSON, EnvelopeJSON, DiagnosticEvents, TxEvents, ContractEvents)
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:356-382` — `lcm_transactions_to_json`: parses LCM via `read_xdr_to_end`, delegates to `extract_transactions_json`, returns single `ConversionResult` with `(json_ptr, json_len)`
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:477-557` — `extract_transactions_json`: builds `Vec<serde_json::Value>` with one object per transaction, calls `serde_json::to_value()` for result, meta, envelope, and each event; final `serde_json::to_string(&result_array)` serializes entire Value tree to string
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:526-531` — per-transaction: `to_value(&result_pair.result)`, `to_value(meta)`, `to_value(envelope)` build intermediate Value trees
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:540-551` — `serde_json::json!({...})` macro builds another Value (map with string keys), pushed to `result_array`
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:556` — `serde_json::to_string(&result_array)` walks the entire Value tree and writes JSON string — second full traversal of the data
- `cmd/stellar-rpc/internal/methods/get_transactions.go:292-322` — Go iterates `rustTxns`, copies scalar fields and assigns `json.RawMessage` fragments directly into `protocol.TransactionInfo` — no further processing of the JSON content

### Findings

**Two confirmed inefficiencies in the hot path:**

**1. Rust-side double traversal via `serde_json::Value` intermediate:**

The current code serializes each component in two steps:
- Step A: `serde_json::to_value(&xdr_type)` — walks the XDR type tree and allocates a `serde_json::Value` DOM (many small heap allocations: `Value::Object`, `Value::Array`, `Value::String`, `Value::Number` nodes)
- Step B: `serde_json::to_string(&result_array)` — walks the entire Value DOM tree and writes JSON characters to a String buffer

With direct serialization (`serde_json::to_vec(&xdr_type)` or `to_string(&xdr_type)`), each component would be serialized in one step directly from the XDR type to a byte buffer, eliminating the Value tree entirely. The XDR types already implement `serde::Serialize` (confirmed: `to_value()` works, which requires the same trait).

Estimated savings: For a ledger with 20 transactions and ~1 MB total JSON output, the Value tree construction adds ~2-5 ms (many small allocations + pointer chasing) and the second traversal adds ~1-3 ms. Per 10-ledger page: ~30-80 ms saved. Against ~100-300 ms total Rust processing: ~10-27%.

**2. Go-side `json.Unmarshal` scanning:**

`json.Unmarshal(jsonBytes, &txns)` on line 74 must scan every byte of the JSON to find per-transaction and per-field boundaries. Although `json.RawMessage` fields avoid deep decoding, the scanner still:
- Processes every byte through a state machine (~200-500 MB/s, slower than raw memcpy)
- Allocates separate `[]byte` slices for each `json.RawMessage` field (copying bytes from the `C.GoBytes` buffer into new heap allocations)
- Parses the outer array structure, object field names, and scalar values

For 1 MB JSON per ledger: ~2-5 ms. For 10-ledger page: ~20-50 ms.
This also creates GC pressure from ~1000+ small allocations (200 txns × 5+ RawMessage fields).

With structured FFI returning pre-separated byte buffers, Go would receive each fragment via individual `C.GoBytes` calls — same total memcpy but no scanner overhead and no double-copy (each `json.RawMessage` would be the `C.GoBytes` allocation directly, not a sub-copy of a larger buffer).

**Combined estimated savings: ~50-130 ms per 200-tx page against ~150-400 ms total = ~13-33%.**

**Comparison with prior fail results:** Fail/006 (eliminating 2 input copies) saved ~2.8% — unmeasurable. Fail/007 (eliminating 1 output copy + 2 scans) saved ~3% — unmeasurable. This hypothesis targets fundamentally larger savings: eliminating the Value tree intermediate (which affects every byte of every component, not just a copy) and the Go json.Unmarshal (which scans every byte through a state machine). The estimated 13-33% is 4-10× larger than the ~3% from fail/006 and fail/007, making it more likely to cross the measurement threshold.

**No correctness concerns:** The XDR types implement `serde::Serialize`, so `serde_json::to_vec()` produces identical JSON to `serde_json::to_value()` + `serde_json::to_string()`. Scalar fields (hash, application_order, fee_bump, successful) can be returned as C types. JSON fragment fields can be returned as `(ptr, len)` byte buffers using the existing `ConversionResult` pattern.

### PoC Guidance

- **Target code**:
  - `cmd/stellar-rpc/lib/xdr2json/src/lib.rs`: Refactor `extract_transactions_json` to serialize each component directly via `serde_json::to_vec()` instead of `to_value()`. Define a new `#[repr(C)]` struct (e.g., `LCMTransactionResult`) holding scalar fields (hash as `*mut c_char`, application_order as `i32`, fee_bump/successful as `bool`) and per-component byte buffers (`result_json_ptr/len`, `meta_json_ptr/len`, `envelope_json_ptr/len`). For events, use `(*mut *mut u8, *mut size_t, count)` arrays. Define `LCMTransactionsResult` holding `(*mut LCMTransactionResult, count, error)`. Update `lcm_transactions_to_json` to return the new type.
  - `cmd/stellar-rpc/lib/xdr2json.h`: Add new C struct definitions matching the Rust types. Add `free_lcm_transactions_result` declaration.
  - `cmd/stellar-rpc/internal/xdr2json/conversion.go`: Replace `LCMTransactionsToJSON` to read the structured FFI result directly into `[]LCMTransactionJSON` without `json.Unmarshal`. Extract scalar fields from C types. Extract JSON fragments via `C.GoBytes` per fragment.
- **Change description**: Replace the JSON-string bridge in `lcm_transactions_to_json` with structured FFI output. Rust serializes each component directly to bytes (one-pass, no Value tree), returns per-transaction structured data with scalar fields and pre-separated JSON fragment buffers. Go reads these directly without `json.Unmarshal`.
- **Correctness check**: Existing tests in `get_transactions_test.go` exercise `processChunksJSON` via `getTransactions` with `format=json`. The output JSON shapes must remain identical. Run `make go-test` and `cargo test`.
- **Benchmark focus**: Measure `getTransactions` latency at 100-175 RPS with `format=json` on Soroban-heavy ledgers. Expect 10-25% p50 improvement from combined Value tree elimination and json.Unmarshal elimination. Also measure Rust-side heap allocation reduction (Value tree nodes no longer allocated).
