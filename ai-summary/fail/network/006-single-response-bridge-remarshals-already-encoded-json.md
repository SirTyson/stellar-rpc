# H003: Single-response POST requests re-marshal an already encoded JSON-RPC envelope

**Date**: 2026-04-07
**Subsystem**: network
**Severity**: Low
**Impact**: response copy / allocation
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once the bridge has already built the final JSON-RPC response bytes for a non-batch `getTransactions` call, it should write those bytes directly to the HTTP response. The single-response path should not run another `json.Marshal` over the already encoded envelope.

## Mechanism

`Bridge.serveInternal` first turns each `jrpc2.Response` into `msg := json.Marshal(rsp)`, stores that `msg` in `results []json.RawMessage`, and then `encodeResponses` passes the same raw message to `writeJSON`. `writeJSON` immediately executes `json.Marshal(obj)` again, so the already final JSON-RPC envelope is scanned and copied a second time before it is written. On the large, ordinary single-request `getTransactions` path, that extra marshal is pure overhead.

## Trigger

Issue normal single-request `getTransactions` POSTs with large JSON pages and compare current behavior with a fast path that skips `writeJSON` for `len(rsps) == 1 && !isBatch`, sets the headers directly, and writes the existing `msg` bytes. Watch allocation counts and the time spent in `encoding/json` on the response-write side.

## Target Code

- `github.com/creachadair/jrpc2@v1.3.5/jhttp/bridge.go:Bridge.serveInternal:131-149` — builds `msg := json.Marshal(rsp)` for each response
- `github.com/creachadair/jrpc2@v1.3.5/jhttp/bridge.go:Bridge.encodeResponses:162-167` — passes a single `json.RawMessage` response into `writeJSON`
- `github.com/creachadair/jrpc2@v1.3.5/jhttp/getter.go:writeJSON:138-150` — re-marshals `obj` even when it is already a fully encoded `json.RawMessage`
- `github.com/creachadair/jrpc2@v1.3.5/base.go:(*Response).MarshalJSON:164-170` — the first marshal already returns the correct JSON-RPC envelope bytes

## Evidence

The common `getTransactions` HTTP request is a single POST, not a batch. In that case the bridge already has final response bytes before `encodeResponses` runs, yet it still calls back into `encoding/json` for a second full pass over the same payload.

## Anti-Evidence

This only removes one response-side pass, so the upside is smaller than eliminating the entire local bridge round-trip. Batch responses still need array assembly, so the optimization is mainly for the dominant single-request path.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated
**Failed At**: reviewer

### Trace Summary

Traced the complete response path through the jrpc2 bridge: `serveInternal` (bridge.go:131-139) marshals each `*jrpc2.Response` into `json.RawMessage`, then `encodeResponses` (bridge.go:162-168) passes that `json.RawMessage` into `writeJSON` (getter.go:138-151), which calls `json.Marshal(obj)` again. However, `json.Marshal` on a `json.RawMessage` does NOT perform a full re-serialization — Go's `json.RawMessage` implements `json.Marshaler` with a `MarshalJSON()` that returns the bytes as-is, and the encoder then runs only a `compact` validation pass (O(n) scan, not O(n) serialization). More critically, all target code resides entirely in the third-party `github.com/creachadair/jrpc2` library.

### Code Paths Examined

- `jrpc2@v1.3.3/jhttp/bridge.go:serveInternal:131-139` — marshals each `*jrpc2.Response` via `json.Marshal(rsp)`, stores as `json.RawMessage` in `results` slice
- `jrpc2@v1.3.3/jhttp/bridge.go:encodeResponses:162-168` — for single non-batch response, calls `writeJSON(w, http.StatusOK, rsps[0])` where `rsps[0]` is `json.RawMessage`
- `jrpc2@v1.3.3/jhttp/getter.go:writeJSON:138-151` — calls `json.Marshal(obj)` on the `any`-boxed `json.RawMessage`, sets Content-Type/Content-Length headers, writes response
- `cmd/stellar-rpc/internal/jsonrpc.go:NewJSONRPCHandler:325-329` — stellar-rpc creates the bridge via `jhttp.NewBridge(...)` and wraps it with network middlewares; no customization point for response encoding
- `go.mod:3` — project uses `jrpc2 v1.3.3`, not v1.3.5 as the hypothesis claims

### Why It Failed

Two independent reasons make this NOT_VIABLE:

1. **All target code is in a third-party library**: Every function identified in the hypothesis (`writeJSON`, `encodeResponses`, `serveInternal`) lives in `github.com/creachadair/jrpc2/jhttp/`. The OUT_OF_SCOPE rules explicitly exclude "Changes to third-party dependencies or external libraries." Stellar-rpc uses the `Bridge` as a black box via `jhttp.NewBridge()` with no hooks or extension points for response encoding. Fixing this would require either modifying the upstream library or reimplementing the entire bridge — neither is a targeted performance optimization.

2. **The "re-marshal" characterization is inaccurate**: `json.Marshal` on `json.RawMessage` does not re-serialize the JSON. `json.RawMessage` implements `json.Marshaler` — its `MarshalJSON()` returns the raw bytes unchanged. The Go encoder then runs `compact` (a validation/byte-copy pass), not a full serialization pass. The overhead is a single O(n) byte scan plus one allocation, which is substantially cheaper than the hypothesis implies. For a typical getTransactions response, this adds microseconds to low single-digit milliseconds — well below the Informational threshold for a meaningful optimization.

### Lesson Learned

Hypotheses targeting code in third-party dependencies (`github.com/creachadair/jrpc2`) should be filtered out early since the OUT_OF_SCOPE rules exclude changes to external libraries. Additionally, `json.Marshal` on `json.RawMessage` in Go is a lightweight compact/validate pass, not a full re-serialization — this is a common misconception that should be verified before hypothesizing about double-marshaling overhead.
