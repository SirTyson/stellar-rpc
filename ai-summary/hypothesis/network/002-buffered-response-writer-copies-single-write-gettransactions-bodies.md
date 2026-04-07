# H002: `bufferedResponseWriter` copies single-write `getTransactions` bodies that are already fully materialized

**Date**: 2026-04-07
**Subsystem**: network
**Severity**: Low
**Impact**: response copy / allocation
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When the downstream JSON-RPC bridge has already built the full HTTP response body as one immutable `[]byte`, the HTTP timeout wrapper should delay committing that body without copying it again. The common single-write `getTransactions` path should not pay a second full-memory pass just to move bytes from one in-process buffer into another.

## Mechanism

`jhttp.writeJSON` marshals the final JSON-RPC envelope into `bits` and issues exactly one `w.Write(bits)`. `bufferedResponseWriter.Write` always does `append(w.buffer, buf...)`, which copies the entire multi-megabyte `getTransactions` response into a new slice even though the bridge never reuses `bits` after the write and `WriteOut` later writes the same bytes again to the real socket. A first-write ownership fast path would eliminate one full-body copy and the associated allocation pressure for the hottest response shape this endpoint emits.

## Trigger

Run large `getTransactions` requests (`xdrFormat=json`, `pagination.limit` near 200) and compare baseline against a version of `bufferedResponseWriter` that adopts the first write buffer by reference when it is the only write. Measure response-path allocations and p50/p95 latency at the highest zero-error load level.

## Target Code

- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:69-90` — `bufferedResponseWriter.Write` always copies the provided bytes into its own buffer
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:97-118` — `WriteOut` later writes that copied buffer to the real `http.ResponseWriter`
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/jhttp/getter.go:138-150` — `writeJSON` marshals once and performs a single `w.Write(bits)` call on the HTTP path
- `cmd/stellar-rpc/internal/jsonrpc.go:337-374` — all `getTransactions` HTTP traffic goes through the timeout wrapper that owns `bufferedResponseWriter`

## Evidence

The bridge’s HTTP writer path is single-shot: it computes `bits`, sets `Content-Length`, writes headers, and then writes the body once. The timeout wrapper then copies that same already-complete body into `bufferedResponseWriter.buffer`, so every large `getTransactions` response pays a redundant full-body memcpy before the final socket write.

## Anti-Evidence

This only helps responses that really are single-write and whose body slice is not mutated after `Write` returns; other writers would still need the existing copy semantics. Prior work already showed that response-copy optimizations can be hard to measure in noisy futurenet runs, so this is likely a Low-severity win rather than a throughput ceiling change.
