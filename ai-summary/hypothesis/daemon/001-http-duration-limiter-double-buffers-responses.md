# H001: HTTP Duration Limiter Double-Buffers getTransactions Responses

**Date**: 2026-04-07
**Subsystem**: daemon
**Severity**: Medium
**Impact**: latency / allocation churn
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When `getTransactions` completes before its configured deadline, the daemon should serialize the JSON-RPC response once and write those bytes directly to the client socket. Large successful responses should not be copied into an intermediate buffer solely to support a timeout path that almost never fires on the success case.

## Mechanism

`NewJSONRPCHandler` wraps the JSON-RPC bridge in `MakeHTTPRequestDurationLimiter`, and that limiter always routes the response through `bufferedResponseWriter`. For `getTransactions`, which can return 50 transactions by default and up to 200 at the configured max, this means every response body is appended into a growable `[]byte` and then copied again on `WriteOut`, adding size-proportional CPU and GC work on the hot path even though the method-specific JSON-RPC limiter has already bounded handler execution.

## Trigger

Issue a steady `getTransactions` workload that returns large payloads (for example, `format=json` or high-limit XDR responses near the 50/200 transaction caps) and compare CPU/alloc profiles before and after bypassing the HTTP response buffer for JSON-RPC success paths.

## Target Code

- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:bufferedResponseWriter.Write:88-90` — every write appends response bytes into an extra buffer.
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:(*httpRequestDurationLimiter).ServeHTTP:124-186` — allocates the buffer and only flushes after the downstream handler returns.
- `cmd/stellar-rpc/internal/jsonrpc.go:NewJSONRPCHandler:368-389` — installs the HTTP duration limiter around all JSON-RPC traffic, including `getTransactions`.

## Evidence

`bufferedResponseWriter` stores the entire body in memory and `WriteOut` later writes that buffer to the real `ResponseWriter`, so successful requests necessarily pay an extra copy. The default config keeps this wrapper enabled (`max-request-execution-duration = 25s`) while `getTransactions` already has a tighter method-specific limit (`max-get-transactions-execution-duration = 5s`), so the common successful path pays the buffering cost even when no HTTP-level timeout handling is needed.

## Anti-Evidence

The outer HTTP limiter does prevent partial responses from escaping if the request times out after the JSON-RPC handler starts writing, so the buffer is not pure dead code. If the bridge already materializes the full response body internally, the incremental win may be smaller than the raw double-copy suggests.
