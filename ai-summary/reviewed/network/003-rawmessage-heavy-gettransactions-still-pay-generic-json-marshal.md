# H003: RawMessage-heavy `getTransactions` replies still pay generic `encoding/json` marshaling in `directBridge`

**Date**: 2026-04-07
**Subsystem**: network
**Severity**: Medium
**Impact**: response serialization CPU / allocation
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

For `getTransactions` JSON responses, most of the heavy payload is already pre-rendered as `json.RawMessage` slices before control returns to the network layer. The bridge should serialize that response with a single append-style pass into the final JSON-RPC success envelope, instead of sending the entire `GetTransactionsResponse` back through generic reflective `encoding/json` marshaling and then copying the result again into an envelope buffer.

## Mechanism

`batchConvertTransactionsToJSON` has already converted envelope/result/meta/event blobs into `json.RawMessage` fields on each `protocol.TransactionInfo`, but `directBridge.serveInternal` still calls `json.Marshal(result)` on the full `protocol.GetTransactionsResponse`. That forces `encoding/json` to walk every `TransactionInfo` and nested `Events` field reflectively, build an intermediate `resultBytes` buffer, and then hand those bytes to `directBridgeSuccessResponse`, which copies them again into the JSON-RPC envelope. A `GetTransactionsResponse`-specific encoder (for example via `MarshalJSON` or an append-based bridge helper) could write scalars plus existing raw subdocuments straight into the final response buffer, eliminating the intermediate marshal buffer and much of the generic reflection overhead.

## Trigger

Issue large `getTransactions` requests with `xdrFormat=json`, `pagination.limit=200`, and ledgers dense with events. Compare baseline against a version that special-cases `protocol.GetTransactionsResponse` in the direct bridge or its protocol type marshaling, and measure response-path allocations plus p50/p95 latency.

## Target Code

- `cmd/stellar-rpc/internal/directbridge.go:108-112` — generic `json.Marshal(result)` still runs for successful replies before envelope construction
- `cmd/stellar-rpc/internal/directbridge.go:130-141` — the success envelope builder immediately copies that marshaled body into a second buffer
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/protocols/rpc/get_transactions.go:25-90` — `TransactionInfo`, `Events`, and `GetTransactionsResponse` are dominated by `json.RawMessage` fields and have no specialized marshaler
- `cmd/stellar-rpc/internal/methods/json.go:105-208` — the method layer already finishes the expensive XDR-to-JSON work before the bridge sees the response

## Evidence

The method code is already paying to produce raw JSON for the dominant per-transaction subdocuments, so the remaining bridge marshal is mostly packaging existing JSON with scalar metadata. The current success path nevertheless rebuilds the entire response through generic `encoding/json`, which creates a large intermediate buffer and repeats a structural walk over every transaction before the bridge's manual envelope copy even begins.

## Anti-Evidence

`encoding/json` is already reasonably efficient on `json.RawMessage`-heavy structs, so this is not a theoretical 2x win; the likely gain is in the 5-20% range only on the largest JSON pages. The custom encoder also has to preserve the exact current field order/escaping/`omitempty` behavior for both JSON and XDR-format responses, which raises implementation risk compared with a pure copy-elimination change.

---

## Review

**Verdict**: VIABLE
**Severity**: Medium
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full path from `directBridge.serveInternal` (line 108) through Go's `encoding/json` `marshalerEncoder` to the `appendCompact` function in `encoding/json/indent.go`. Confirmed that `json.Marshal` on a `GetTransactionsResponse` containing `json.RawMessage` fields does NOT simply memcpy the raw bytes — it runs every byte through `appendCompact`, a per-byte JSON scanner/validator with HTML-escape checks, before copying into an intermediate `encodeState` buffer. The result is then copied again by `directBridgeSuccessResponse` into the final JSON-RPC envelope. This creates three buffer allocations and two full validation/copy passes of the multi-MB payload.

### Code Paths Examined

- `cmd/stellar-rpc/internal/directbridge.go:108-112` — `json.Marshal(result)` where result is `protocol.GetTransactionsResponse` returned as `any` from the jrpc2 handler wrapper
- `cmd/stellar-rpc/internal/directbridge.go:130-141` — `directBridgeSuccessResponse` pre-sizes a new buffer and copies the entire `resultBytes` into the JSON-RPC envelope (third allocation, second full copy)
- `/usr/local/go/src/encoding/json/encode.go:marshalerEncoder` — calls `MarshalJSON()` on each `json.RawMessage` field, then passes result through `appendCompact(out, b, opts.escapeHTML)` — escapeHTML is true by default
- `/usr/local/go/src/encoding/json/indent.go:appendCompact` — per-byte loop: creates a JSON scanner, calls `scan.step(scan, c)` for every byte (indirect function call through state machine), checks HTML-escape conditions (`<`, `>`, `&`, U+2028/U+2029) on every byte
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/protocols/rpc/get_transactions.go:42-90` — `TransactionDetails` has 6 `json.RawMessage` fields (EnvelopeJSON, ResultJSON, ResultMetaJSON, DiagnosticEventsJSON, plus Events sub-struct with TransactionEventsJSON and ContractEventsJSON)
- `cmd/stellar-rpc/internal/methods/json.go:105-211` — `batchConvertTransactionsToJSON` confirms all RawMessage fields are pre-rendered via xdr2json FFI before the bridge sees the response
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.5/handler/handler.go:Wrap` — confirms handler returns `(any, error)` via `vals[0].Interface()`, so the bridge receives a boxed `GetTransactionsResponse` value

### Findings

The hypothesis correctly identifies a real inefficiency, and the actual overhead is **larger than the hypothesis anticipated** due to `appendCompact`:

**Current cost for a 200-transaction JSON response (~6 MB of RawMessage content):**
1. **`appendCompact` pass**: Per-byte scanner validation + HTML-escape checks on all RawMessage bytes. At ~1-2 ns/byte (indirect function call + state machine + branch checks per byte), this costs ~6-12 ms for 6 MB of already-valid, already-compact JSON.
2. **`json.Marshal` return copy**: `encodeState.Bytes()` copies the full buffer for the return value — ~2 ms for 6 MB.
3. **`directBridgeSuccessResponse` copy**: Another full copy into the envelope buffer — ~2 ms for 6 MB.
4. **Total serialization-path overhead**: ~10-16 ms.

**With a custom bridge encoder (bypass `json.Marshal` entirely):**
1. Pre-size one envelope buffer, append field names + scalars + raw RawMessage bytes directly — one memcpy per field, no scanner, no validation.
2. **Total**: ~2-3 ms (dominated by memcpy of RawMessage content).

**Estimated savings**: 8-13 ms per large-page request, or roughly 10-40% of total request latency (depending on DB read + XDR→JSON FFI cost).

**Novelty vs. H011**: H011 proposed pre-encoding the result as `json.RawMessage` to avoid server-side marshal. That relocates work without eliminating it — `marshalerEncoder` would still run `appendCompact` on the pre-encoded bytes. H003 proposes bypassing `json.Marshal` entirely via a bridge-level type switch, which genuinely eliminates the `appendCompact` pass. This is a substantively different mechanism.

**Key constraint**: `MarshalJSON` on `GetTransactionsResponse` would NOT help — the `marshalerEncoder` runs `appendCompact` on the output of `MarshalJSON()` too. The optimization must bypass `json.Marshal` entirely at the bridge level.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/directbridge.go:108-112` — replace `json.Marshal(result)` + `directBridgeSuccessResponse` with a type-switched fast path for `protocol.GetTransactionsResponse`
- **Change description**: Add a function `appendGetTransactionsJSON(buf []byte, resp protocol.GetTransactionsResponse) []byte` that appends the JSON-RPC envelope + response directly into one pre-sized buffer. For each `json.RawMessage` field, append the raw bytes without validation. For scalars (Status, TxHash, Ledger, etc.), use `strconv.AppendInt`/`strconv.AppendUint`/`strconv.AppendQuote`. Handle `omitempty` with nil/len checks. In `serveInternal`, do a type switch: `if resp, ok := result.(protocol.GetTransactionsResponse); ok { ... }`.
- **Critical correctness note**: Do NOT use `MarshalJSON` on the protocol type — `marshalerEncoder` still runs `appendCompact`. The bypass must be at the bridge call site, replacing `json.Marshal(result)` entirely for this type.
- **Field order**: Must match `encoding/json`'s output order (struct declaration order) exactly, including nested `TransactionDetails` and `Events` fields.
- **Correctness check**: Existing `directbridge_test.go` and `get_transactions_test.go` cover the response format; also verify byte-for-byte equivalence with `json.Marshal` output in the PoC benchmark.
- **Benchmark focus**: p50/p95 latency for `getTransactions` with `xdrFormat=json`, `pagination.limit=200`, dense-event ledgers. Expect 10-30% latency reduction for large pages. Also measure allocations via `testing.B.ReportAllocs`.
