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
