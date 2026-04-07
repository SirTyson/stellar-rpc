# H024: Preallocating the timeout buffer from `Content-Length` will remove large-response copy costs

**Date**: 2026-04-07
**Subsystem**: network
**Severity**: Low
**Impact**: response-buffer allocation
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

If `bufferedResponseWriter` knows the final `Content-Length`, preallocating its body buffer should avoid repeated growth and materially reduce the cost of buffering large `getTransactions` responses. A pre-sized buffer would only be worthwhile if the current path is suffering from multiple reallocations or geometric growth copies.

## Mechanism

`jhttp.writeJSON` sets `Content-Length` before writing the body, so it initially looks possible for `bufferedResponseWriter` to read that header and `Grow` its buffer ahead of time. If the body arrived in many chunks, that could eliminate repeated reallocations and produce a measurable response-path improvement.

## Trigger

Patch `bufferedResponseWriter` to inspect `Content-Length` and preallocate `buffer` before the first body write, then compare allocations and latency for large JSON-format `getTransactions` responses.

## Target Code

- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:(*bufferedResponseWriter).Write:88-90` — current append-based buffering
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/jhttp/getter.go:138-150` — `writeJSON` sets `Content-Length` and writes the body

## Evidence

The response writer does not currently use `Content-Length` to size its buffer, and the HTTP bridge does publish the exact final length before the body write. That makes preallocation look plausible without changing semantics.

## Anti-Evidence

`writeJSON` writes the whole body in a **single** `w.Write(bits)` call, and `bufferedResponseWriter.Write` appends into a nil slice. On that path Go allocates once for the final size and copies once; there is no cascade of intermediate reallocations to eliminate. The only meaningful remaining win would be **copy elimination** (taking ownership of the body slice), not pre-sizing the destination.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The hot `getTransactions` HTTP path is already single-write, so preallocation does not remove any extra growth copies. It changes capacity bookkeeping, not the actual full-body memcpy that dominates this layer.

### Lesson Learned

When the downstream writer emits one complete buffer, focus on **whether the copy can be removed**, not on classic slice-growth optimizations. Capacity tuning only matters when the body arrives in multiple chunks.
