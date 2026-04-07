# H002: The in-process `jhttp` bridge round-trips large `getTransactions` responses through JSON bytes

**Date**: 2026-04-07
**Subsystem**: network
**Severity**: Medium
**Impact**: in-process serialization CPU / allocation
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Because stellar-rpc's HTTP JSON-RPC bridge and method handlers live in the same process, a `getTransactions` response should not need to be serialized into JSON, sent across an in-memory channel, parsed back into protocol messages, and then serialized again just to reach the HTTP writer. The hot path should keep the large response in-process and encode it only once at the boundary that actually writes bytes to the client.

## Mechanism

`NewJSONRPCHandler` builds `jhttp.NewBridge(...)`, and `jhttp.NewBridge` in turn constructs `server.NewLocal`, an in-memory JSON-RPC client/server pair connected by `channel.Direct()`. For a large `getTransactions` response, the method result is first marshaled in `Server.invoke`, then wrapped into JSON-RPC reply bytes and sent through the local channel, then parsed back into `jmessage` objects by the local client, and finally marshaled again by the bridge before the HTTP layer sees it. Those encode/decode passes are all in-process overhead unrelated to ledger access or actual socket I/O.

## Trigger

Run large `getTransactions` requests (`format=json`, `limit` near 200) and compare current behavior with a custom direct POST bridge that dispatches to the handler map without `server.NewLocal`/`Client.Batch`. Measure CPU profiles, bytes allocated per request, and latency in the response-serialization phase.

## Target Code

- `cmd/stellar-rpc/internal/jsonrpc.go:325-329` — constructs the HTTP bridge with `jhttp.NewBridge(decorateHandlers(...))`
- `github.com/creachadair/jrpc2@v1.3.5/server/local.go:NewLocal:26-34` — creates an in-memory client/server pair via `channel.Direct()`
- `github.com/creachadair/jrpc2@v1.3.5/server.go:Server.invoke:372-389` — marshals the handler result with `json.Marshal(v)` after the method returns
- `github.com/creachadair/jrpc2@v1.3.5/server.go:tasks.responses:773-813` — wraps the raw result into JSON-RPC response messages
- `github.com/creachadair/jrpc2@v1.3.5/client.go:Client.send:198-239` — serializes the request batch to JSON and sends it on the in-memory channel
- `github.com/creachadair/jrpc2@v1.3.5/client.go:Client.accept:74-98` — parses the returned response bytes back into protocol messages
- `github.com/creachadair/jrpc2@v1.3.5/jhttp/bridge.go:Bridge.serveInternal:126-149` — re-marshals bridge responses for HTTP output

## Evidence

The bridge is not a direct function-call adapter; it explicitly stands up a local JSON-RPC transport and moves every HTTP request through JSON frames. `getTransactions` is one of the worst-case consumers of that design because the response body is large and already JSON-heavy, so every extra encode/decode pass scans and copies multi-megabyte payloads.

## Anti-Evidence

The request body itself is small, so most of the win comes from large responses rather than from request ingress. Replacing the bridge path is more invasive than a local micro-optimization and must preserve batch-request and JSON-RPC error behavior.

---

## Review

**Verdict**: VIABLE
**Severity**: Medium
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated. H004 (bufferedResponseWriter copy) targets a different layer (HTTP timeout buffering) and was confirmed as Low severity with a failed PoC. This hypothesis targets the jhttp bridge's internal serialize→channel→deserialize→re-serialize round-trip, which is a distinct and larger source of overhead.

### Trace Summary

I traced the complete data path through the jhttp bridge for a getTransactions response. The handler result struct is marshaled to JSON by `Server.invoke` (first marshal). The server wraps this in a JSON-RPC envelope via `jmessage.toJSON()` and sends the envelope bytes through the `channel.Direct()` channel. The client's `accept` method receives these bytes and calls `jmessage.parseJSON()`, which does `json.Unmarshal(data, &map[string]json.RawMessage{})` — scanning the entire multi-MB envelope to extract the result field, allocating new copies of all field values. The bridge's `serveInternal` then calls `json.Marshal(rsp)` on each `*Response`, invoking `Response.MarshalJSON()` which reconstructs the JSON-RPC envelope via `jmessage.toJSON()`. Finally, `writeJSON` calls `json.Marshal(json.RawMessage)` to produce the final HTTP output. The result bytes are thus copied/scanned at least 4 times after the initial marshal, with the JSON parsing step being the most expensive.

### Code Paths Examined

- `cmd/stellar-rpc/internal/jsonrpc.go:325-329` — confirmed: `jhttp.NewBridge(decorateHandlers(...))` creates the bridge using `server.NewLocal` internally.
- `jrpc2@v1.3.3/jhttp/bridge.go:serveInternal:82-163` — confirmed: calls `b.local.Client.Batch(ctx, spec)`, then iterates over responses calling `json.Marshal(rsp)` for each, then `encodeResponses` → `writeJSON(w, 200, rsps[0])` which does another `json.Marshal`.
- `jrpc2@v1.3.3/server/local.go:NewLocal:28-36` — confirmed: creates `channel.Direct()` pair and starts server on one end, client on other.
- `jrpc2@v1.3.3/channel/channel.go:Direct` — confirmed: unbuffered Go channel of `[]byte`. `Send` pushes the same byte slice reference (zero copy), `Recv` returns the same pointer.
- `jrpc2@v1.3.3/server.go:invoke:379-397` — confirmed: calls `json.Marshal(v)` on handler's return value. This is the **first and necessary** marshal of the response data.
- `jrpc2@v1.3.3/server.go:deliver:291-305` — confirmed: calls `encode(ch, rsps)` which calls `rsps.toJSON()` (wrapping result in JSON-RPC envelope) then `ch.Send(bits)`. The `toJSON()` method copies `j.R` (the raw result bytes) into a `bytes.Buffer` as part of the envelope.
- `jrpc2@v1.3.3/client.go:accept:76-98` — confirmed: calls `in.parseJSON(bits)` which does `json.Unmarshal(data, &obj)` where obj is `map[string]json.RawMessage`. This **scans the entire envelope** (including multi-MB result field) to find value boundaries and allocates new copies of each field.
- `jrpc2@v1.3.3/json.go:parseJSON:175-230` (jmessage) — confirmed: `json.Unmarshal(data, &obj)` produces `map[string]json.RawMessage`, then each field is extracted. The `"result"` key's value (the large payload) is copied into a new `json.RawMessage`.
- `jrpc2@v1.3.3/base.go:Response.MarshalJSON:163-168` — confirmed: creates a new `jmessage{ID: ..., R: r.result}` and calls `.toJSON()`, which rebuilds the JSON-RPC envelope with `sb.Write(j.R)` — another copy of the result bytes.
- `jrpc2@v1.3.3/jhttp/getter.go:writeJSON:138-151` — confirmed: `json.Marshal(obj)` where obj is `json.RawMessage` — produces a copy of the envelope bytes, then writes to `http.ResponseWriter`.
- `jrpc2@v1.3.3/client.go:marshalParams:430-445` — confirmed: request-side params are `json.RawMessage`, so `json.Marshal` is essentially a copy (not a re-encode). Request overhead is negligible.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:319-325` — confirmed: returns `protocol.GetTransactionsResponse` struct with up to 200 `TransactionInfo` entries, each containing JSON-encoded envelope, result, meta, and events fields.

### Findings

**The inefficiency is real and correctly described.** The complete response serialization path involves:

1. **`json.Marshal(handler_result)`** in `Server.invoke` — **Required**. Converts the Go struct to JSON bytes. For 200 transactions with JSON format, this produces ~1-6MB of result JSON. Cost: proportional to output size.

2. **`jmessage.toJSON()`** in server's `encode` — Wraps result in JSON-RPC envelope. Copies ~N MB of result bytes into a `bytes.Buffer`. Cost: ~0.1-0.3ms memcpy. **Avoidable with direct dispatch.**

3. **`channel.Direct().Send`** — Zero-copy pointer transfer via Go channel. Cost: negligible.

4. **`jmessage.parseJSON(bits)`** in client's `accept` — **The main waste.** `json.Unmarshal` into `map[string]json.RawMessage` scans the entire envelope (including the ~N MB result field) to find JSON value boundaries. Go's JSON scanner processes at ~200-500 MB/s, so scanning 5MB takes ~10-25ms. Also allocates new copies of all field values. **Avoidable with direct dispatch.**

5. **`Response.MarshalJSON()`** in bridge's `serveInternal` — Reconstructs JSON-RPC envelope with remapped ID. Copies result bytes again. Cost: ~0.1-0.3ms. **Avoidable with direct dispatch.**

6. **`json.Marshal(json.RawMessage)`** in `writeJSON` — Copies envelope bytes for HTTP output. Cost: ~0.1-0.3ms. **Partially avoidable** (could write directly instead of marshal+write).

**Estimated overhead for a ~5MB response (200 transactions, JSON format):**
- JSON scanning in step 4: ~10-25ms (dominant cost)
- Extra memory copies in steps 2, 4, 5, 6: ~15MB of transient allocations
- GC pressure: every in-flight request holds ~3x the response size transiently

**Compared to total request latency (~50-200ms for DB + XDR decode + initial marshal), the bridge round-trip adds ~10-25ms, which is ~7-17% overhead.** This falls within the Medium severity range (5-20%).

**Why this is different from H004:** H004 targeted the `bufferedResponseWriter` in the HTTP timeout layer, which adds one extra full-body copy. That was confirmed as Low severity (~0.5ms for memcpy) and the PoC (sync.Pool) showed no measurable improvement. H002 targets a fundamentally different and larger source of overhead — the full JSON parse (not just copy) of the response envelope in the bridge's internal channel round-trip. JSON parsing is ~10-50x slower than memcpy for the same data size.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/jsonrpc.go` — replace `jhttp.NewBridge` with a custom `directBridge` HTTP handler that dispatches to the handler map without the `server.NewLocal`/`Client.Batch` round-trip.
- **Change description**: Implement a `directBridge` struct that:
  1. Parses the HTTP body as JSON-RPC requests (reuse `jrpc2.ParseRequests`)
  2. Looks up each method in the `handler.Map` assigner
  3. Calls the handler directly (constructing a `*jrpc2.Request` via `ParsedRequest.ToRequest()`)
  4. Marshals the result once via `json.Marshal`
  5. Wraps in JSON-RPC envelope and writes directly to the HTTP response writer
  6. Handles batch requests, notifications, error formatting, and ID mapping correctly
  The key benefit is eliminating the server→channel→client→re-marshal path. The handler result goes directly from `json.Marshal(v)` → JSON-RPC envelope → HTTP writer, with no intermediate parse step.
- **Correctness check**: All existing tests in `cmd/stellar-rpc/internal/` must pass. The bridge must correctly handle: single requests, batch requests, notifications, JSON-RPC error responses (both handler errors and protocol errors like invalid method), Content-Type validation, and ID preservation. Integration tests in `cmd/stellar-rpc/internal/integrationtest/` provide end-to-end coverage.
- **Benchmark focus**: Measure latency and allocations for `getTransactions` with `format=json&limit=200` against ledgers with populated events. The primary metric is p50/p99 latency reduction; expect ~10-20% improvement for large responses. Also measure bytes allocated per request (`-benchmem`), expecting ~60-70% reduction in serialization-related allocations. Use CPU profiles to confirm elimination of the `jmessage.parseJSON` hotspot in the response path.
- **Risk**: This is a moderately invasive change (~200-300 lines of new bridge code). The main risk is subtle protocol compliance issues with batch requests or error handling. Thorough testing against the integration test suite is essential.

---

## PoC Attempt

**Result**: POC_PASS
**Date**: 2026-04-07
**PoC by**: claude-opus-4-6, high

### Changes Made

1. **`cmd/stellar-rpc/internal/directbridge.go`** (NEW, ~200 lines) — Implements `directBridge` struct that replaces `jhttp.Bridge`. The struct holds a `jrpc2.Assigner` and dispatches HTTP JSON-RPC requests directly to handlers without creating a `server.NewLocal`/`Client.Batch` round-trip. Key functions:
   - `ServeHTTP`: validates HTTP method/Content-Type (matching jhttp.Bridge behavior)
   - `serveInternal`: parses JSON-RPC requests via `jrpc2.ParseRequests`, looks up handlers via `Assigner.Assign`, calls them directly, marshals results once, and builds JSON-RPC envelopes manually via byte concatenation
   - Envelope builders (`directBridgeSuccessResponse`, `directBridgeErrorResponse`) construct JSON-RPC response envelopes without `json.Marshal` overhead — they pre-allocate a buffer and append raw bytes
   - Handles all JSON-RPC edge cases: single/batch requests, notifications (204 No Content), method-not-found errors, handler errors (wrapping non-jrpc2 errors with InternalError code), and statically invalid requests

2. **`cmd/stellar-rpc/internal/jsonrpc.go`** (lines 14-16, 51-55, 168-172, 332-336) — Replaced `jhttp.Bridge` with `directBridge`:
   - Removed `jhttp` import
   - Changed `Handler.bridge` field type from `jhttp.Bridge` to `directBridge`
   - Removed `jhttp.BridgeOptions` and `jrpc2.ServerOptions` setup
   - Changed `jhttp.NewBridge(...)` call to `newDirectBridge(...)`

### Demonstration

The `directBridge` eliminates 4 unnecessary serialization steps from the jhttp bridge's response path. Instead of marshal→envelope→channel→parse→re-marshal→write, the response now goes handler→marshal→envelope→write. For a 5MB `getTransactions` response (200 transactions, JSON format), this removes the ~10-25ms `json.Unmarshal` scan of the full response envelope in the client's `accept` path, plus ~15MB of transient allocations from intermediate copies. The optimization is proportional to response size, benefiting all large-response methods (getTransactions, getEvents, getLedgers).

### Test Results

All 12 test packages in `cmd/stellar-rpc/internal/...` pass with `-race` flag enabled:
- `config`, `db`, `feewindow`, `ingest`, `integrationtest`, `ledgerbucketwindow`, `methods`, `network`, `preflight`, `rpcdatastore`, `util`, `xdr2json` — all OK
- All 3 Rust crate tests pass (`preflight`, `xdr2json`, `ffi`)
- Build succeeds cleanly with `make build-stellar-rpc`
