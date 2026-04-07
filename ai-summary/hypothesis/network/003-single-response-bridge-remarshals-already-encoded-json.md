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
