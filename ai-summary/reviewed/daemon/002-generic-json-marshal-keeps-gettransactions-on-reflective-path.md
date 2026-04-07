# H002: Generic json.Marshal Keeps getTransactions on a Reflective Serialization Path

**Date**: 2026-04-07
**Subsystem**: daemon
**Severity**: Low
**Impact**: JSON serialization CPU
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

The hottest large-response RPC in the daemon should serialize its fixed response schema through a specialized path that appends known fields directly, especially once the inner transaction fields are already available as `string` and `json.RawMessage`. A `getTransactions` page with up to 200 `TransactionInfo` entries should not pay full generic `encoding/json` reflection and dispatch costs if a stable schema-specific encoder can emit the same bytes.

## Mechanism

`directBridge` always calls `json.Marshal(result)` on the method return value, so `getTransactions` pages go through the generic `encoding/json` machinery even though the response shape is fixed and known ahead of time. Because `protocol.GetTransactionsResponse` is a large nested struct of repeated `TransactionInfo`, strings, slices, and `json.RawMessage` fields, a bridge-side specialized encoder can avoid much of the per-field reflection, tag lookup, and dynamic dispatch that the generic marshal path repeats on every large page.

## Trigger

Benchmark `getTransactions` with large response pages in both `xdr` and `json` modes, then compare the current bridge to a version that type-switches on `protocol.GetTransactionsResponse` and appends the response JSON directly instead of calling generic `json.Marshal`.

## Target Code

- `cmd/stellar-rpc/internal/directbridge.go:108-112` — current bridge always marshals the returned value with generic `json.Marshal`.
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/protocols/rpc/get_transactions.go:42-90` — fixed `TransactionDetails`, `TransactionInfo`, and `GetTransactionsResponse` schema that a specialized encoder can target.

## Evidence

The bridge has no method-specific serialization fast path: every successful request converges on `json.Marshal(result)`. The `getTransactions` protocol types are static structs with a stable field order and many `json.RawMessage` members whose payloads are already serialized, which makes them a good fit for a custom append-based encoder implemented in daemon code rather than repeated reflective marshaling.

## Anti-Evidence

Any encoder still has to copy the actual string and raw JSON payload bytes into the final response buffer, so this cannot remove the dominant payload-size cost. The measurable win depends on `encoding/json` showing up in profiles once the larger DB/XDR/FFI inefficiencies are reduced.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full serialization path from handler return through directBridge to HTTP response. The getTransactions handler returns `protocol.GetTransactionsResponse` (containing up to 200 `TransactionInfo` entries), which flows through `decorateHandlers` as `any`, then hits `json.Marshal(result)` at `directbridge.go:108`. Go 1.25's `encoding/json` caches type encoders (so per-call reflection overhead is minimal), but critically, every `json.RawMessage` field is processed through `appendCompact()` — a byte-by-byte JSON state machine that validates and re-copies already-valid JSON. This is the true source of waste, not reflection.

### Code Paths Examined

- `cmd/stellar-rpc/internal/directbridge.go:108` — `json.Marshal(result)` on the handler's `any` return value; the sole serialization bottleneck
- `cmd/stellar-rpc/internal/jsonrpc.go:88-115` — `decorateHandlers` wraps handlers; result flows as `any` without type-specific fast paths
- `cmd/stellar-rpc/internal/methods/get_transactions.go:140-187,277-307` — handler builds `GetTransactionsResponse`; in JSON mode, populates `json.RawMessage` fields from trusted Rust FFI output; in XDR mode, populates `string` fields with base64
- `/usr/local/go/src/encoding/json/encode.go:479-493` — `marshalerEncoder` calls `MarshalJSON()` then `appendCompact()` for every `json.RawMessage` field
- `/usr/local/go/src/encoding/json/indent.go:51-89` — `appendCompact` runs a per-byte JSON scanner (`scan.step` function pointer call per byte) plus HTML-escape checks on every byte
- `/usr/local/go/src/encoding/json/encode.go:209` — `json.Marshal` defaults to `escapeHTML: true`, adding per-byte `<>&` checks
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/protocols/rpc/get_transactions.go:42-90` — `TransactionDetails` has 6 `json.RawMessage` fields plus nested `Events` struct with 2 more `json.RawMessage` slice fields

### Findings

**Corrected Mechanism**: The hypothesis attributes the waste to "reflection and dispatch costs," but Go's `encoding/json` caches per-type encoder functions after the first call — there is no per-invocation reflection. The actual waste is `appendCompact()`, which is called for every `json.RawMessage` field and processes each byte through a JSON state machine (function pointer call per byte via `scan.step`) plus HTML-escape scanning. This is redundant because the RawMessage payloads are produced by the trusted Rust xdr2json FFI converter and are already valid, compact JSON.

**Impact Analysis by Format**:

- **JSON mode** (`xdrFormat: "json"`): Each `TransactionInfo` has up to 8 `json.RawMessage` fields (EnvelopeJSON, ResultJSON, ResultMetaJSON, DiagnosticEventsJSON, plus Events sub-fields). For 200 transactions with an estimated ~50KB of RawMessage data each, that's ~10MB processed through `appendCompact` at ~200-400MB/s (due to per-byte function pointer dispatch) = **25-50ms** in validation alone. A custom encoder copying these bytes as-is (memcpy at ~10GB/s) would cost ~1ms. **Potential savings: 24-49ms per large JSON-mode response.**

- **XDR/base64 mode** (default, `xdrFormat: ""` or `"base64"`): Fields are base64 `string` values encoded via `appendString`, which uses a simple table lookup per byte (~3-5GB/s). For 200 transactions × ~15KB base64 avg = 3MB, this costs <1ms. **Savings would be <1ms — negligible.**

**Severity Assessment**: The optimization is only meaningful for JSON-mode responses. The default format is XDR/base64, where savings are negligible. For JSON-mode with large responses, the savings are real (potentially 5-20% of total request latency), but JSON mode must be explicitly requested. Overall severity is Low, though it could reach Medium for heavy JSON-mode workloads.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/directbridge.go:108` — add a type-switch before `json.Marshal(result)` that detects `protocol.GetTransactionsResponse` and uses a custom append-based encoder
- **Change description**: Implement `appendGetTransactionsResponse(dst []byte, resp protocol.GetTransactionsResponse) []byte` that writes the JSON directly: iterate fields, write string literals for keys, use `strconv.AppendInt`/`AppendUint` for numbers, `append` for `json.RawMessage` bytes (without validation), and `appendString` for string fields that need escaping. This bypasses `appendCompact` for all RawMessage fields.
- **Correctness check**: `TestGetTransactions` and integration tests in `cmd/stellar-rpc/internal/integrationtest/` cover both XDR and JSON mode responses. The custom encoder must produce byte-identical JSON output to `json.Marshal` for all field combinations (empty/populated, XDR/JSON mode, with/without events).
- **Benchmark focus**: Benchmark `json.Marshal(result)` vs the custom encoder for a `GetTransactionsResponse` with 200 transactions in JSON mode. Target metric: marshal time. Expected improvement: 50-80% reduction in marshal time for JSON-mode responses (from ~25-50ms to ~5-10ms). For XDR mode, expect minimal difference (<1ms). Use `testing.B` with realistic payloads from testnet captures or synthetic data with representative RawMessage sizes.
