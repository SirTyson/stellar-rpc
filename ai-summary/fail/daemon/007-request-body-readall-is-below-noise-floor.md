# H007: directBridge Request-Body Buffering Is a Meaningful getTransactions Bottleneck

**Date**: 2026-04-07
**Subsystem**: daemon
**Severity**: Low
**Impact**: request parse copying
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Request parsing should avoid unnecessary body copies when the incoming payload is large enough for that copy to matter. For `getTransactions`, normal request bodies should be small enough that response generation — not request ingestion — dominates the endpoint cost.

## Mechanism

I investigated whether `directBridge` reading the full HTTP body with `io.ReadAll(req.Body)` before `jrpc2.ParseRequests(body)` could be a notable hot-path copy. That would only be a meaningful optimization target if `getTransactions` requests were themselves large or complex enough for request buffering to rival the response-side DB/XDR/JSON work.

## Trigger

Inspect the request shape and compare request-parse costs with end-to-end profiles for large `getTransactions` responses.

## Target Code

- `cmd/stellar-rpc/internal/directbridge.go:58-65` — reads the full request body before parsing JSON-RPC requests.
- `cmd/stellar-rpc/internal/jsonrpc.go:35-39,372-384` — daemon caps HTTP request size at 512 KB and routes all RPC calls through the same handler stack.
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/protocols/rpc/get_transactions.go:10-15` — request shape is just `startLedger`, optional pagination, and format.

## Evidence

The bridge does buffer the whole request before dispatch, so there is a real extra copy on the request path.

## Anti-Evidence

`getTransactions` request payloads are tiny in normal use: a couple of integers, an optional cursor string, and a format selector. The expensive part of the method is generating and serializing the response page, so even eliminating request buffering would be well below the pipeline's measurable-improvement threshold for this endpoint.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The copy is real but sits on a tiny input. `getTransactions` spends its time in ledger fetch, transaction decoding, FFI/JSON work, and response writing, so request-body buffering is below noise floor for the targeted endpoint.

### Lesson Learned

For `getTransactions`, response-size work is the right place to hunt. Request-path copies only matter if the API accepts large request payloads, which this method does not.
