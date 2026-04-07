# H010: Request-side JSON-RPC bridge re-encoding is too small to matter for `getTransactions`

**Date**: 2026-04-07
**Subsystem**: network
**Severity**: Low
**Impact**: request-side JSON handling
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

For request-side bridge work to be a viable `getTransactions` optimization, the ingress JSON handling would need to account for a meaningful share of end-to-end request cost. Small control-plane marshaling and bookkeeping should not outrank the large response-side work already confirmed elsewhere in this subsystem.

## Mechanism

I investigated whether the local `jhttp` bridge wastes enough CPU before the handler runs because it parses the HTTP request into `ParsedRequest`, rebuilds `[]jrpc2.Spec`, re-marshals the params into a local JSON-RPC request, and tracks a pending response object. The bridge really does perform that extra request-side JSON handling, but `getTransactions` request bodies are tiny while the costly part of this endpoint is the multi-megabyte response path, so the ingress reroute is too small to be a standalone optimization.

## Trigger

Benchmark ordinary single-request `getTransactions` POST bodies and compare current behavior with a version that dispatches directly from `ParsedRequest` to the handler without `Client.Batch`, focusing only on request-side CPU and allocations before response generation begins.

## Target Code

- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/jhttp/bridge.go:(Bridge).serveInternal:100-148` — builds `[]jrpc2.Spec` and routes the request through `Client.Batch`
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/jhttp/bridge.go:(Bridge).parseHTTPRequest:151-159` — reads the HTTP body and parses it into `ParsedRequest`
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/client.go:(*Client).send:196-245` — marshals the local request batch and allocates pending response tracking
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/client.go:(*Client).marshalParams:428-444` — re-marshals `Spec.Params` for the local hop
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/json.go:10-30` — converts parsed wire messages into `ParsedRequest`

## Evidence

The bridge absolutely does extra local request work: HTTP bytes are parsed into `ParsedRequest`, then the same params are repackaged into `Spec`, then `Client.send()` marshals a new JSON-RPC request message for the in-memory channel. That makes the request path a distinct part of the bridge overhead, not just the response path.

## Anti-Evidence

`getTransactions` requests are small and fixed-shape, usually only dozens to a few hundred bytes, so these extra marshaling steps are tiny compared with the bridge's already-documented large-response parse and copy overhead. The best-case win here is swamped by response serialization, logging, and buffering costs.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The request-side reroute exists, but the request payload is so small that removing it would not produce a meaningful `getTransactions` latency or RPS gain by itself.

### Lesson Learned

For bridge work on `getTransactions`, the only promising opportunities are the response-side passes over large payloads; ingress JSON handling is too small to matter.
