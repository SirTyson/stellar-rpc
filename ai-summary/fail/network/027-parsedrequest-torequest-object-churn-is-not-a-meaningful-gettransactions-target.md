# H027: `ParsedRequest.ToRequest()` object churn is not a meaningful `getTransactions` target

**Date**: 2026-04-07
**Subsystem**: network
**Severity**: Low
**Impact**: request-object setup
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

If direct-bridge request-object construction were a meaningful hot-path cost for `getTransactions`, replacing `ParsedRequest.ToRequest()` with a lighter-weight dispatch path should measurably reduce request latency or allocation pressure.

## Mechanism

`serveInternal` parses the HTTP body into `ParsedRequest` values and then constructs a `*jrpc2.Request` via `ToRequest()` before handing the call to the method handler. At first glance this looks like avoidable per-request object churn in the direct-bridge path.

## Trigger

Compare baseline against a version of the direct bridge that dispatches from `ParsedRequest` directly, skipping `ToRequest()` and any related request-object construction, while sending high-rate `getTransactions` traffic.

## Target Code

- `cmd/stellar-rpc/internal/directbridge.go:64-93` — parses requests and then calls `parsed.ToRequest()` for handler dispatch
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/json.go:36-58` — `ParsedRequest` and `ToRequest()`
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/json.go:275-279` — `fixID` only filters `"null"` IDs
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/base.go:52-66` — `Request` contains only tiny `id`, `method`, and `params` fields

## Evidence

The direct bridge does construct a second small request object for every call, and that object creation is visible in the current request path immediately before handler dispatch.

## Anti-Evidence

`ToRequest()` reuses the already-parsed `Params` buffer instead of copying it, and `fixID` is just a tiny null check. The resulting `Request` is only a few small fields, while `getTransactions` spends its time in ledger reads, XDR/JSON conversion, large response marshaling, and response buffering. That makes this object churn far below the measurable bar for the endpoint.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The only work `ToRequest()` adds is construction of a tiny wrapper struct and a trivial `fixID` normalization; it does not deep-copy request parameters. For `getTransactions`, that cost is dwarfed by response-side work on multi-megabyte payloads.

### Lesson Learned

In the current direct-bridge design, request-object setup is still request-side noise. For `getTransactions`, novel network wins remain on the response path: large-result marshaling, envelope construction, batch aggregation, and the scope of queue accounting.
