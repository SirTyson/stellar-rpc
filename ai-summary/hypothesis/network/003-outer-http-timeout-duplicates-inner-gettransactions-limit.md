# H003: Successful `getTransactions` requests still pay a redundant outer HTTP timeout wrapper

**Date**: 2026-04-07
**Subsystem**: network
**Severity**: Low
**Impact**: duplicate timeout/buffering overhead on successful requests
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once `getTransactions` already has a method-specific backlog limiter and a method-specific execution timeout, successful requests should not also allocate a second HTTP timeout goroutine, timer set, context, and full-response buffer just to protect the in-process bridge's encode-and-write phase. The common success path should use the cheapest wrapper stack that still enforces the intended timeout semantics.

## Mechanism

`NewJSONRPCHandler` wraps the per-method `getTransactions` handler with `MakeJrpcRequestDurationLimiter`, then wraps the whole HTTP bridge again with `MakeHTTPRequestDurationLimiter`. Every successful `getTransactions` call therefore pays for the outer HTTP duration limiter's timers, goroutine, context, channel, `bufferedResponseWriter`, and extra body copy even though the expensive method body is already guarded by the inner 5s JSON-RPC timeout and the outer 25s limit mostly covers in-process bridge serialization plus the final HTTP write.

## Trigger

Benchmark large successful `getTransactions` responses (`format=json`, high `limit`) with the current stack and compare against a variant that bypasses the outer HTTP duration limiter for JSON-RPC POST requests whose method already has an inner execution limit. Watch admitted-request latency, allocation volume, and bytes copied per request; the issue is present if the wrapper-only savings remain measurable without changing method results or timeout behavior for genuinely long-running handlers.

## Target Code

- `cmd/stellar-rpc/internal/jsonrpc.go:291-323` — `getTransactions` is already wrapped by `MakeJrpcRequestDurationLimiter`
- `cmd/stellar-rpc/internal/jsonrpc.go:337-361` — the full bridge is wrapped again by `MakeHTTPRequestDurationLimiter`
- `cmd/stellar-rpc/internal/config/options.go:525-531` — global HTTP request timeout defaults to 25s
- `cmd/stellar-rpc/internal/config/options.go:575-579` — `getTransactions` method timeout defaults to 5s
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:124-194` — the outer HTTP limiter allocates timers, a derived context, a goroutine, and a buffered response writer for every admitted request
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:69-119` — `bufferedResponseWriter` captures the full response body before copying it back to the real writer

## Evidence

The outer HTTP timeout wrapper is always on the success path, not just the overload path. Prior investigation already showed that the body buffer copy by itself is real but too small to justify a standalone pooling fix; removing the whole outer wrapper for timed JSON-RPC methods is a different optimization because it also eliminates the extra goroutine, timers, context, channel, and buffer allocation that every successful request currently pays.

## Anti-Evidence

The outer wrapper still protects code outside the method handler, including bridge serialization, panic handling, and the final HTTP write, so any bypass must preserve those semantics for the endpoints that actually need them. The likely win is modest because the bridge round-trip and method body still dominate total `getTransactions` cost.
