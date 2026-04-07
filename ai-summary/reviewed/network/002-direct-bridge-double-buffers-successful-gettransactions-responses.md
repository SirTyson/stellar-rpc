# H002: directBridge double-buffers each successful `getTransactions` reply before the HTTP timeout layer sees it

**Date**: 2026-04-07
**Subsystem**: network
**Severity**: Low
**Impact**: response copy / peak heap
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once the direct bridge has produced the final JSON-RPC success body for a large `getTransactions` response, it should not need to hold a separate full-size `result` buffer and then copy that buffer into a second full-size envelope buffer. The bridge should materialize the success payload once.

## Mechanism

`serveInternal` first does `json.Marshal(result)`, producing a full `[]byte` for the `GetTransactionsResponse`, and then `directBridgeSuccessResponse` allocates another `[]byte` and copies the entire marshaled result into `{"jsonrpc":"2.0","id":...,"result":...}`. For a multi-megabyte `getTransactions` success body, that is one avoidable full-body memcpy plus simultaneous residency of both large buffers before the HTTP duration limiter copies the envelope again into `bufferedResponseWriter`. A one-pass envelope encoder could remove one full-size allocation and one full-size copy from every successful JSON-format response.

## Trigger

Issue large `getTransactions` requests with `xdrFormat=json` and `pagination.limit=200`, then compare baseline allocations and latency against a version that encodes the JSON-RPC success envelope in one pass instead of `json.Marshal(result)` followed by `directBridgeSuccessResponse`.

## Target Code

- `cmd/stellar-rpc/internal/directbridge.go:108-112` — full response body is first materialized by `json.Marshal(result)`
- `cmd/stellar-rpc/internal/directbridge.go:128-141` — `directBridgeSuccessResponse` allocates a second buffer and copies the whole result into it
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:88-90` — the HTTP timeout wrapper later copies the direct-bridge output again into its response buffer
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/protocols/rpc/get_transactions.go:74-90` — the response type is large because each transaction carries multiple raw JSON fields

## Evidence

The current direct-bridge success path is explicitly two-stage: marshal the result, then wrap it by copying it into a new envelope slice. That means every successful large `getTransactions` response pays at least one extra full-memory pass before it even reaches the timeout buffer.

## Anti-Evidence

Any implementation still needs one final contiguous response body for the HTTP write, so the optimization only removes one of several response copies. Replacing the manual wrapper with a one-pass encoder may also introduce some `encoding/json` bookkeeping overhead, which likely keeps this in the Low-severity range.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full HTTP `getTransactions` success path through `directBridge.serveInternal` (directbridge.go:58-126). After the handler returns a `protocol.GetTransactionsResponse` (a struct dominated by `json.RawMessage` fields), `json.Marshal(result)` (line 108) uses Go's pooled `encodeState` to encode the struct, then copies the internal buffer into a new `resultBytes` allocation (~5MB for 200 transactions). `directBridgeSuccessResponse` (line 130-141) then allocates a second ~5MB buffer with exact capacity and copies all of `resultBytes` into the JSON-RPC envelope via `append`. After this, `resultBytes` becomes garbage. The envelope is then written via `directBridgeWriteJSON` → `w.Write(data)` into the `bufferedResponseWriter` (a third copy, targeted by a separate reviewed hypothesis). The second copy (resultBytes → envelope) is eliminable by encoding directly into the envelope buffer.

### Code Paths Examined

- `cmd/stellar-rpc/internal/directbridge.go:108-112` — `json.Marshal(result)` returns `resultBytes`: internally uses pooled `encodeState`, encodes `GetTransactionsResponse` (mostly `json.RawMessage` copy-pass), copies pool buffer into new `[]byte`. One ~5MB allocation + one ~5MB memcpy.
- `cmd/stellar-rpc/internal/directbridge.go:130-141` — `directBridgeSuccessResponse` pre-allocates `buf` with exact capacity (`len(envelope_template) + len(idJSON) + len(result)`), then copies all of `result` via `append(buf, result...)`. One ~5MB allocation + one ~5MB memcpy. After return, the `resultBytes` from line 108 has no remaining references and becomes GC-eligible.
- `cmd/stellar-rpc/internal/directbridge.go:211-216` — `directBridgeWriteJSON` calls `w.Write(data)` which, under the timeout wrapper, hits `bufferedResponseWriter.Write` (line 88-90) — a separate copy targeted by reviewed hypothesis 002-buffered-response-writer.
- `cmd/stellar-rpc/internal/jsonrpc.go:332-336` — Confirmed `directBridge` (not jhttp.Bridge) is the active HTTP bridge. All HTTP traffic routes through it.
- `cmd/stellar-rpc/internal/jsonrpc.go:359-365` — Wiring: `directBridge` → `httpRequestDurationLimiter` (global timeout with `bufferedResponseWriter`) → `BacklogHTTPQLimiter` (outer).
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/protocols/rpc/get_transactions.go:42-90` — `TransactionDetails` contains 6 `json.RawMessage` fields (EnvelopeJSON, ResultJSON, ResultMetaJSON, DiagnosticEventsJSON) plus nested `Events` with more `json.RawMessage` slices. `json.Marshal` on this struct is a copy-pass for the dominant payload.

### Findings

The inefficiency is real and on the hot path. The copy chain for a successful `getTransactions` JSON response is:

1. **encodeState → resultBytes** (inside `json.Marshal`, line 108): pooled internal buffer → new allocation. ~5MB copy.
2. **resultBytes → envelope** (inside `directBridgeSuccessResponse`, line 112/130-141): new allocation + full memcpy. ~5MB copy. **← This is the eliminable copy.**
3. **envelope → bufferedResponseWriter.buffer** (inside `Write`, requestdurationlimiter.go:88-90): another allocation + full memcpy. ~5MB copy. (Separate optimization target.)

By encoding the handler result directly into the envelope buffer, step 2 is eliminated entirely — `json.NewEncoder` (or `json.MarshalAppend` if available in Go 1.25) would write the `encodeState` contents directly into the pre-prefixed envelope buffer, skipping the intermediate `resultBytes` allocation.

**Impact estimate:**
- One ~5MB allocation eliminated per large request (reduces GC pressure)
- One ~5MB memcpy eliminated per large request (~0.25ms at 20 GB/s single-core bandwidth)
- Peak heap reduced by ~5MB during response construction (both buffers no longer coexist)
- As percentage of total request latency (20-100ms): ~0.25-1.25% — Low severity

**Distinction from prior work:**
- Fail 004 (sync.Pool for bufferedResponseWriter): targeted copy 3, saved allocation only, not the memcpy. Failed benchmarks.
- Fail 011 (pre-encode as json.RawMessage): proposed eliminating the marshal entirely, but marshal is already a copy-pass on RawMessage-dominated structs. Different mechanism.
- Fail 024 (Content-Length preallocation): targeted bufferedResponseWriter slice growth — not applicable since it's single-write.
- Reviewed 002 (bufferedResponseWriter zero-copy): targets copy 3 (envelope → timeout buffer). Independent and composable with this optimization.

**Correctness analysis:**
- `json.NewEncoder.Encode` produces identical JSON output to `json.Marshal` for the same input struct. The only difference is a trailing newline, which must be handled (replace with closing brace or trim).
- The `encodeState` pool is shared — both `Marshal` and `Encoder.Encode` use it. No new pool contention.
- `directBridgeSuccessResponse` function signature changes from `(id string, result json.RawMessage) json.RawMessage` to a new function like `directBridgeMarshalSuccess(id string, result any) (json.RawMessage, error)` — the caller at line 108-112 must be refactored to call the combined function directly with `result` (the `any` return from the handler).
- No thread safety concerns — all within a single goroutine in `serveInternal`.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/directbridge.go` — replace lines 108-112 (`json.Marshal` + `directBridgeSuccessResponse`) with a combined one-pass encoder
- **Change description**: Create a new function `directBridgeMarshalSuccess(id string, result any) (json.RawMessage, error)` that: (1) writes the envelope prefix (`{"jsonrpc":"2.0","id":<id>,"result":`) into a `bytes.Buffer`, (2) uses `json.NewEncoder(&buf).Encode(result)` to write the marshaled result directly into the same buffer, (3) replaces the trailing newline with `}`. This eliminates the intermediate `resultBytes` allocation and copy. An alternative approach is `json.MarshalAppend` if available in Go 1.25.
- **Correctness check**: Existing tests in `directbridge_test.go` (if present) and `requestdurationlimiter_test.go`. Also verify JSON output byte-for-byte equivalence for several response sizes (empty, small, large).
- **Benchmark focus**: Measure allocations (`-benchmem`) for the `directBridge.serveInternal` path with large `getTransactions` responses (200 transactions, JSON format). Expect one fewer ~5MB allocation per request. Latency improvement (~0.25ms) may be below futurenet benchmark noise — allocation metrics are more reliable signal.
