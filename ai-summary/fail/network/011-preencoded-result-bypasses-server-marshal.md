# H001: `getTransactions` can return a pre-encoded result instead of a giant response struct

**Date**: 2026-04-07
**Subsystem**: network
**Severity**: Medium
**Impact**: serialization CPU / peak heap
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once `getTransactions` has already produced JSON-form transaction payloads, the network path should not need to walk and marshal a huge `protocol.GetTransactionsResponse` object again just to hand bytes to JSON-RPC. The hot path should encode the final result exactly once, then reuse those bytes when the JSON-RPC server and logging wrapper need the method result.

## Mechanism

`processTransactionsInLedger()` already converts the expensive parts of each transaction into `json.RawMessage` values (`resultJson`, `envelopeJson`, `resultMetaJson`, diagnostic events, and event payloads), but `getTransactionsByLedgerSequence()` still returns a large Go struct slice that `jrpc2.Server.invoke` later marshals in full at the network boundary. A `getTransactions`-specific `jrpc2.Handler` that assembles the final result object bytes once and returns `json.RawMessage` would remove that post-handler marshal pass, and it would also make `logResponse()`'s current success-path marshal degrade to a cheaper raw-message copy instead of another struct walk.

## Trigger

Request `getTransactions` with `format=json` and `pagination.limit` near the configured maximum against ledgers with populated events. Compare current behavior with a version that assembles the final result JSON inside the handler and returns `json.RawMessage` directly, measuring CPU in `encoding/json`, bytes allocated per request, and peak heap under concurrency.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:(transactionsRPCHandler).processTransactionsInLedger:152-207` — already produces per-transaction JSON fragments that could be appended into a final result buffer
- `cmd/stellar-rpc/internal/methods/get_transactions.go:(transactionsRPCHandler).getTransactionsByLedgerSequence:267-327` — materializes the full `[]protocol.TransactionInfo` response graph before returning
- `cmd/stellar-rpc/internal/methods/get_transactions.go:NewGetTransactionsHandler:330-341` — currently returns the generic wrapped handler instead of a `getTransactions`-specific exact-signature handler
- `cmd/stellar-rpc/internal/jsonrpc.go:logResponse:124-141` — re-marshals every successful handler result before the bridge sees it
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/server.go:(*Server).invoke:377-395` — marshals the returned handler value with `json.Marshal(v)` after the method completes

## Evidence

`GetTransactionsResponse` is unusually friendly to pre-encoding: the expensive nested blobs are already `json.RawMessage` in `protocol.TransactionDetails`, so the handler is doing the hard JSON conversion work before the network layer runs. The remaining server-side marshal is therefore mostly a large top-level array/object walk plus copies of already-serialized fragments, and it happens after the handler has already allocated the full response graph in memory.

## Anti-Evidence

This does not remove the separate local-bridge response round-trip that was already identified in reviewed H002, so it cannot eliminate all response-side overhead by itself. The XDR path and small pages will benefit less, and building the final result bytes inside the handler is more invasive than ordinary wrapper tweaks.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated (distinct from H009 which covers request-side reflection, H006 which covers bridge re-marshal, and reviewed H001 which covers logResponse debug marshal)
**Failed At**: reviewer

### Trace Summary

Traced the complete marshal chain: `getTransactionsByLedgerSequence` returns `GetTransactionsResponse` struct → `decorateHandlers` closure receives it as `any` → `logResponse` calls `json.Marshal(response)` unconditionally for successful requests (marshal #1) → result returned to `jrpc2.Server.invoke` → invoke calls `json.Marshal(v)` (marshal #2, server.go:395). The proposal is to pre-encode the result as `json.RawMessage` in the handler so that marshals #1 and #2 become cheap compact passes instead of struct reflection walks. However, this requires the handler itself to either call `json.Marshal` (moving the struct walk earlier, zero net savings) or manually assemble JSON (adding an O(N) data copy), while the downstream compact passes on `json.RawMessage` still scan every byte.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:199-304` — returns `(protocol.GetTransactionsResponse, error)` containing `[]TransactionInfo` slice with up to 200 items
- `cmd/stellar-rpc/internal/methods/get_transactions.go:NewGetTransactionsHandler:306-318` — uses `handler.New()` which wraps the function via reflection; the return value becomes `any` for the jrpc2 handler chain
- `github.com/creachadair/jrpc2@v1.3.3/handler/handler.go:Wrap:135-239` — decodeOut (line 224-229) extracts the return value as `any` via `vals[0].Interface()`
- `cmd/stellar-rpc/internal/jsonrpc.go:decorateHandlers:83-104` — calls `h(ctx, r)` getting `result` as `any`, passes to `logResponse` at line 102, returns `result` at line 103
- `cmd/stellar-rpc/internal/jsonrpc.go:logResponse:124-142` — unconditionally calls `json.Marshal(response)` at line 135 when `status == "ok"` (marshal #1)
- `github.com/creachadair/jrpc2@v1.3.3/server.go:invoke:379-395` — calls `json.Marshal(v)` at line 395 on the handler's return value (marshal #2)
- `go-stellar-sdk@v0.3.0/protocols/rpc/get_transactions.go:42-90` — `TransactionDetails` contains `json.RawMessage` fields (EnvelopeJSON, ResultJSON, ResultMetaJSON, DiagnosticEventsJSON) which are already pre-serialized JSON; `json.Marshal` on the struct walks fields via reflection and runs a compact pass on each RawMessage field

### Why It Failed

Three independent reasons make this NOT_VIABLE:

1. **Pre-encoding moves work rather than eliminating it.** To return `json.RawMessage`, the handler must either (a) call `json.Marshal(GetTransactionsResponse{...})` explicitly — identical cost to the current invoke marshal, just relocated — or (b) build JSON manually with `bytes.Buffer`, adding an O(N) data copy (~1-5ms for a 5MB response) plus significant maintenance burden. Neither approach produces a net reduction in total work.

2. **invoke's compact pass is unavoidable and dominates.** `json.Marshal(json.RawMessage)` in invoke (server.go:395) still runs a full O(N) compact/validation scan over every byte. Go's `json.RawMessage.MarshalJSON()` returns the raw bytes, then the encoder calls `compact` which walks byte-by-byte. The struct reflection overhead eliminated (~200-500μs for ~3000 field accesses with cached field info) is negligible compared to the data scanning cost and is offset by the handler-level pre-assembly work.

3. **The logResponse waste is already addressed by a simpler fix.** The unconditional `json.Marshal(response)` in `logResponse` is already identified as VIABLE in `reviewed/network/001-debug-response-marshal-runs-at-info.md`, where the fix is simply gating on log level — completely eliminating the marshal at non-debug levels without any pre-encoding. Once that fix is applied, the only remaining marshal is invoke's (which cannot be bypassed without modifying the third-party jrpc2 library, which is OUT_OF_SCOPE).

Additionally, H004's PoC (sync.Pool for buffered response writer) empirically demonstrated that per-request allocation/copy optimizations in this network layer produce no measurable improvement under load testing, with the optimized version performing within noise or worse at all RPS levels tested.

### Lesson Learned

When `json.RawMessage` fields dominate a struct, `json.Marshal` on that struct is already efficient — the encoder copies pre-serialized bytes through a compact pass with minimal reflection overhead. Pre-encoding the entire response only relocates the marshal work to the handler without eliminating the downstream compact pass in `jrpc2.Server.invoke`. Optimization efforts on the response-serialization path should focus on eliminating unnecessary marshal calls entirely (as in the log-level gating fix) rather than trying to make individual marshals cheaper.
