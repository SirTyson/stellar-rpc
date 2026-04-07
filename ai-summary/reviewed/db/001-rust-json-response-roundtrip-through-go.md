# H001: Raw-LCM JSON fast path round-trips Rust JSON back through Go

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Medium
**Impact**: CPU / allocation / JSON serialization overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once the Rust raw-LCM extractor has produced per-transaction JSON for a `getTransactions` page, the Go path should carry those bytes straight to the final RPC response. The hot path should not serialize a full JSON payload in Rust, parse that payload back into Go structs, and then serialize it again when the JSON-RPC response is written.

## Mechanism

The current JSON fast path does exactly that round-trip. `extract_transactions_json()` builds a full JSON array string in Rust, `LCMTransactionsToJSON()` copies that string into Go and `json.Unmarshal`s it into `[]LCMTransactionJSON`, and the handler later re-encodes the enclosing `protocol.GetTransactionsResponse` back to JSON on the HTTP path. For large `result/meta/envelope` payloads, especially Soroban-heavy pages, that extra Rust-serialize -> Go-parse -> Go-serialize cycle can consume a meaningful slice of end-to-end latency even after the XDR round-trip was removed.

## Trigger

Call `getTransactions` with `format=json` over ledgers whose transactions have large `meta` and event payloads, then compare CPU/allocation profiles for the raw-LCM path. The expected hot spots are Rust `serde_json::to_string`, Go `json.Unmarshal` in `LCMTransactionsToJSON`, and the final response marshal in the bridge/server path.

## Target Code

- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:extract_transactions_json:477-556` — Rust serializes the whole ledger's transaction array to a JSON string.
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:LCMTransactionsToJSON:45-78` — Go copies the Rust JSON bytes and unmarshals them into `[]LCMTransactionJSON`.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:processChunksJSON:257-339` — handler consumes the decoded structs and rebuilds `protocol.TransactionInfo`.
- `cmd/stellar-rpc/internal/directbridge.go:serveInternal:108-112` — result object is marshaled again for the outbound JSON-RPC response.

## Evidence

`LCMTransactionsToJSON()` currently performs `C.GoBytes(...)` followed by `json.Unmarshal(...)` on every raw-LCM JSON ledger result (`cmd/stellar-rpc/internal/xdr2json/conversion.go:71-76`). On the Rust side, `extract_transactions_json()` materializes a `Vec<serde_json::Value>` and then `serde_json::to_string`s the whole array (`cmd/stellar-rpc/lib/xdr2json/src/lib.rs:505-556`). The adjacent `directBridge` optimization explicitly calls out that avoiding a large `json.Unmarshal`/re-marshal cycle matters for `getTransactions` responses (`cmd/stellar-rpc/internal/directbridge.go:15-19`), which is the same class of waste this internal Rust->Go handoff still pays.

## Anti-Evidence

The endpoint still needs one final JSON serialization at the HTTP boundary, so this cannot remove all JSON work. A fix likely needs a new FFI/result shape (for example, per-field raw JSON slices or preframed response fragments) and must preserve the exact wire format expected by the existing RPC tests.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated. Distinct from reviewed/db/005 (which implemented the raw-LCM FFI path) and fail/db/003 (which was about the old per-transaction XDR round-trip). This hypothesis addresses a remaining inefficiency WITHIN the already-implemented raw-LCM path.

### Trace Summary

Traced the complete raw-LCM JSON path: `getTransactionsByLedgerSequence` (line 438) branches on `FormatJSON`, calls `BatchGetLedgersBySequences` for raw LCM chunks, then `processChunksJSON` → `LCMTransactionsToJSON`. In Rust, `lcm_transactions_to_json` parses the LCM XDR, builds `Vec<serde_json::Value>` per transaction, and serializes the full array via `serde_json::to_string`. Back in Go, `C.GoBytes` copies the entire JSON blob, then `json.Unmarshal` scans it into `[]LCMTransactionJSON` — extracting scalars and copying payload fields as `json.RawMessage`. The handler copies these into `protocol.TransactionInfo` structs, which are ultimately `json.Marshal`ed at `directBridge.serveInternal:108`. Critically, `json.RawMessage` fields are written byte-for-byte during this final marshal — they are NOT re-serialized.

### Code Paths Examined

- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:extract_transactions_json:477-556` — Rust builds `serde_json::Value` objects for result/meta/envelope/events via `serde_json::to_value()`, then serializes the entire array to a String via `serde_json::to_string`. Two Rust-side serialization steps: XDR struct → Value DOM → String.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:lcm_transactions_to_json:356-382` — FFI entry point: copies the JSON string bytes into a `ConversionResult` with `json_ptr`/`json_len`.
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:LCMTransactionsToJSON:45-78` — Go copies entire Rust JSON via `C.GoBytes` (line 71), then `json.Unmarshal` into `[]LCMTransactionJSON` (line 74). Payload fields (ResultJSON, MetaJSON, EnvelopeJSON, events) are `json.RawMessage` — Go's decoder scans to find field boundaries but does not deeply parse them.
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:29-40` — `LCMTransactionJSON` struct definition confirms all heavy fields are `json.RawMessage`.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:processChunksJSON:257-339` — Copies `json.RawMessage` references from `LCMTransactionJSON` into `protocol.TransactionInfo` fields (lines 311-318). These are slice header copies, not data copies.
- `cmd/stellar-rpc/internal/directbridge.go:serveInternal:108` — `json.Marshal(result)` on the `GetTransactionsResponse`. For `json.RawMessage` fields, Go writes raw bytes directly — no re-encoding of payload data.
- `github.com/stellar/go-stellar-sdk@v0.4.0/protocols/rpc/get_transactions.go:42-80` — `TransactionDetails` uses `json.RawMessage` for EnvelopeJSON, ResultJSON, ResultMetaJSON, DiagnosticEventsJSON. These are all `omitempty`, so absent fields add zero cost.

### Findings

**The hypothesis overstates the "Go-serialize" component.** The claimed "Rust-serialize → Go-parse → Go-serialize" cycle is more accurately described as:

1. **Rust serialize** (real cost): `serde_json::to_value` + `serde_json::to_string` — two-step serialization on the Rust side. This is the dominant cost and cannot be eliminated.
2. **C.GoBytes copy** (real but small): copies the entire JSON blob from C heap to Go heap. For a typical 200-transaction page with moderate payloads (~1-5MB), this is ~10-50µs.
3. **Go json.Unmarshal scanning** (real but small): scans the JSON to find field boundaries. For `json.RawMessage` fields, Go tokenizes but does not deeply parse — it just finds matching braces/brackets. Cost: ~20-100µs for 1-5MB of JSON.
4. **Per-field json.RawMessage allocations** (real): creates ~6+ individual `[]byte` slices per transaction for the payload fields, plus event slices. For 200 transactions, ~1200+ allocations. Cost: ~10-50µs.
5. **Final json.Marshal** (NOT re-serialization): `json.RawMessage` fields are written byte-for-byte without re-encoding. The marshal only constructs the outer struct envelope with field names and scalar values. Cost: ~20-50µs for the struct traversal.

**Total Go-side overhead: ~60-250µs per page** (C.GoBytes + Unmarshal + allocations + final Marshal envelope). Against a total request latency of 5-7ms (measured in success/db/001), this represents **~1-4% of total latency**.

**The `json.RawMessage` design was a deliberate optimization** in the PoC that landed (reviewed/db/005). It ensures payload data isn't re-parsed — the Rust JSON bytes for result/meta/envelope/events pass through Go as opaque byte slices. The only true overhead is the scanning to find field boundaries (necessary because Rust returns a single JSON array, not per-field chunks).

**A fix would require significant FFI restructuring.** The proposed "per-field raw JSON slices" approach would require Rust to return a binary envelope with length-prefixed per-field JSON chunks, plus separate metadata (hash, app order, fee bump, successful) in a structured format. Go would then slice the buffer without parsing. This eliminates the `json.Unmarshal` step but adds FFI complexity. The field name mismatch (Rust: `result`/`meta`/`envelope` vs wire: `resultJson`/`resultMetaJson`/`envelopeJson`) prevents direct JSON passthrough.

**Severity downgrade rationale**: The hypothesis claims Medium (5-20%). The actual overhead is ~1-4% because `json.RawMessage` prevents the "Go-serialize" step the hypothesis assumes. The dominant cost is Rust XDR→JSON conversion, not Go JSON handling. A fix could save 1-4% latency, which falls in the Low range (<5% but measurable with profiling).

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:lcm_transactions_to_json` — change return format from single JSON array string to a binary envelope with per-transaction structured chunks. `cmd/stellar-rpc/internal/xdr2json/conversion.go:LCMTransactionsToJSON` — replace `json.Unmarshal` with direct buffer slicing. `cmd/stellar-rpc/internal/methods/get_transactions.go:processChunksJSON` — adapt to consume the new structured output.
- **Change description**: Have Rust return per-transaction metadata (hash, app_order, fee_bump, successful as fixed-size fields) plus length-prefixed JSON slices for each payload field (result, meta, envelope, diagnostic_events, tx_events, contract_events). Go parses only the fixed-size metadata and slices the buffer at length-prefix boundaries, avoiding `json.Unmarshal` entirely.
- **Correctness check**: Existing tests in `cmd/stellar-rpc/internal/methods/get_transactions_test.go` cover the JSON path. The wire-format output must remain byte-identical. Fee-bump and event grouping behavior must be preserved.
- **Benchmark focus**: Measure Go-side allocation rate (allocs/op) and `json.Unmarshal` CPU contribution via `pprof`. Expect ~1-4% latency improvement on `getTransactions` JSON requests. The allocation reduction (~1200 fewer per-field allocations per page) may have a secondary GC pressure benefit at high RPS.
