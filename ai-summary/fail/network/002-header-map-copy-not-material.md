# H002: Header-map copying in `bufferedResponseWriter` is not a material getTransactions cost

**Date**: 2026-04-06
**Subsystem**: network
**Severity**: Low
**Impact**: minor allocation
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

The HTTP timeout wrapper should not spend meaningful time duplicating headers for `getTransactions`. A viable optimization here would need header handling to be a noticeable fraction of end-to-end request work.

## Mechanism

I investigated whether `makeBufferedResponseWriter` and `WriteOut` spend enough time copying header maps to justify a dedicated optimization. The actual behavior does clone and later restore the header map, but JSON-RPC responses only carry a tiny, stable header set, so this copy work is dwarfed by response-body serialization and buffering.

## Trigger

Profile large `getTransactions` HTTP responses and isolate time spent in header cloning/restoration versus response-body generation and copying.

## Target Code

- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:makeBufferedResponseWriter:75-81` — clones the original header map
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:(*bufferedResponseWriter).WriteOut:97-103` — clears and recopies headers before flushing

## Evidence

The code does allocate a fresh `http.Header` map and uses `maps.Copy` twice per request, so there is real work here. That made it worth checking while evaluating the buffering layer.

## Anti-Evidence

`getTransactions` performance is dominated by the body path: transaction extraction, JSON/XDR conversion, and the full-response buffer. The headers are few and tiny, so eliminating only the map copies would not plausibly yield a measurable improvement.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-06
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The header-copy work is real but too small; the body-buffer copy and timeout machinery are the meaningful costs in this code path.

### Lesson Learned

When a wrapper touches both headers and body, the body path is the place to look first for `getTransactions`, especially because this endpoint can return very large payloads.
