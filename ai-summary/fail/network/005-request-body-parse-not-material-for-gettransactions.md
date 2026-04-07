# H005: Request-body parsing is too small to matter for `getTransactions`

**Date**: 2026-04-07
**Subsystem**: network
**Severity**: Low
**Impact**: request-ingress CPU
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

The HTTP bridge's request parsing should not consume a meaningful fraction of `getTransactions` time. A viable optimization here would need request ingress work to be large enough to measurably change end-to-end latency or throughput.

## Mechanism

I investigated whether `Bridge.parseHTTPRequest` and `jrpc2.ParseRequests` do enough work on every `getTransactions` call to justify optimization. The actual behavior does read the full request body and parse JSON once, but `getTransactions` request bodies are tiny compared with the response path: they contain only the method name and a small pagination/format object, so ingress parsing is dominated by the downstream ledger scan and response serialization phases.

## Trigger

Compare current behavior with a hand-written request parser for ordinary `getTransactions` POST bodies, focusing on bytes allocated and CPU in request ingress versus response generation.

## Target Code

- `github.com/creachadair/jrpc2@v1.3.5/jhttp/bridge.go:Bridge.parseHTTPRequest:151-159` — reads the full HTTP body and calls `jrpc2.ParseRequests`
- `github.com/creachadair/jrpc2@v1.3.5/json.go:ParseRequests:14-30` — parses the JSON-RPC request payload into parsed requests

## Evidence

There is real per-request work here: `io.ReadAll(req.Body)` copies the request bytes and `ParseRequests` unmarshals them. That made it worth checking while tracing the bridge.

## Anti-Evidence

The request size cap is 512 KB, but real `getTransactions` requests are far smaller than that and their bodies are tiny compared with the potentially multi-megabyte responses. Any savings here would be lost in noise next to the response-side marshaling and buffering costs already present in this path.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The bridge does parse every request body, but `getTransactions` request bodies are too small for that cost to matter relative to ledger work and large response serialization.

### Lesson Learned

For `getTransactions`, novel performance findings need to target response-side encoding, buffering, or overload control; request ingress parsing is too small to clear the measurable-impact bar.
