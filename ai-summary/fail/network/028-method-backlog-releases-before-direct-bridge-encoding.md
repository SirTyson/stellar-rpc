# H001: Method backlog releases large `getTransactions` responses before direct-bridge encoding finishes

**Date**: 2026-04-07
**Subsystem**: network
**Severity**: Medium
**Impact**: queue accounting / unbounded response-serialization fan-out
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

`request-backlog-get-transactions-queue-limit` should bound the full method-specific cost of a `getTransactions` request until its JSON-RPC success body is ready for the HTTP layer. A slot should not be released while the same request is still doing large response marshaling and envelope construction for `format=json`.

## Mechanism

`queueLimiter.Handle` decrements the per-method backlog counter as soon as `durationLimiter.Handle` returns, but the current direct bridge performs `json.Marshal(result)` and `directBridgeSuccessResponse(...)` only **after** that return. For large `getTransactions` responses, this means the expensive response-serialization phase runs outside the method-specific queue limit, so a low `request-backlog-get-transactions-queue-limit` can still fan out multiple concurrent multi-megabyte marshals and envelope copies. That extra CPU and heap pressure is significant because `GetTransactionsResponse` is dominated by `json.RawMessage` fields and prior network findings already showed that even one additional full marshal of this payload is measurable.

## Trigger

Set `request-backlog-get-transactions-queue-limit` to `1` or `2`, issue many concurrent `getTransactions` requests with `xdrFormat=json` and `pagination.limit=200`, and profile the response phase. Compare baseline against a version that keeps the method backlog slot occupied through direct-bridge success encoding, and look for fewer concurrent `encoding/json` marshals plus lower tail latency.

## Target Code

- `cmd/stellar-rpc/internal/jsonrpc.go:313-330` — per-method stack installs `queueLimiter.Handle` as the decorated handler entry point
- `cmd/stellar-rpc/internal/network/backlogQ.go:109-142` — per-method backlog slot is released by the deferred decrement when `Handle` returns
- `cmd/stellar-rpc/internal/directbridge.go:92-112` — direct bridge marshals the returned handler result only after `queueLimiter.Handle` has finished
- `cmd/stellar-rpc/internal/directbridge.go:128-141` — success-envelope builder copies the large marshaled result into a second buffer
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/protocols/rpc/get_transactions.go:74-90` — `GetTransactionsResponse` / `TransactionInfo` carry large `json.RawMessage` payloads per transaction

## Evidence

The queue limiter wraps only the handler call chain; it does not cover the direct bridge's success encoding. In the current code, the per-method backlog defer fires before `directBridge` allocates `resultBytes` and before it copies those bytes into the JSON-RPC envelope, so the method-specific queue no longer reflects all expensive `getTransactions` work.

## Anti-Evidence

The global HTTP backlog still caps total outstanding requests, so this does not create unbounded server-wide concurrency. The effect is also much smaller for `xdr` responses or small pagination limits, where the uncovered response-serialization phase is shorter.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated (related to condensed H007 on timeout-boundary placement, but this targets the queue boundary, which is a distinct mechanism)
**Failed At**: reviewer

### Trace Summary

Traced the full call chain from HTTP ingress through `directBridge.serveInternal` (directbridge.go:58-126) into the per-method handler stack. Confirmed the mechanism: `queueLimiter.Handle` (backlogQ.go:109-142) releases its slot via deferred atomic decrement before `json.Marshal(result)` (directbridge.go:108) and `directBridgeSuccessResponse` (directbridge.go:112) execute. However, the default per-method queue limit is 1000 and the default handler timeout is 5s, making the uncovered marshaling phase negligible in steady-state concurrency accounting. The proposed fix would hold slots longer without eliminating any work.

### Code Paths Examined

- `cmd/stellar-rpc/internal/jsonrpc.go:315-330` — per-method stack: `durationLimiter` wraps handler, `queueLimiter` wraps `durationLimiter`, `decorateHandlers` wraps `queueLimiter`; confirmed queue slot covers only the JRPC handler call, not the HTTP-level marshaling
- `cmd/stellar-rpc/internal/directbridge.go:92-112` — after `h(req.Context(), jrequest)` returns (line 93), `json.Marshal(result)` (line 108) and `directBridgeSuccessResponse` (line 112) run outside the queue slot; for `GetTransactionsResponse` with `json.RawMessage` fields, `json.Marshal` is a copy-pass operation (~5-10ms for 200 transactions)
- `cmd/stellar-rpc/internal/network/backlogQ.go:109-142` — `BacklogJrpcQLimiter.Handle` deferred decrement fires on return; confirmed slot is released before caller (directBridge) does any marshaling
- `cmd/stellar-rpc/internal/config/options.go:489-491` — default `request-backlog-get-transactions-queue-limit` is **1000**
- `cmd/stellar-rpc/internal/config/options.go:434` — default global HTTP backlog is **5000**
- `cmd/stellar-rpc/internal/config/options.go:578-579` — default per-method timeout is **5 seconds**

### Why It Failed

1. **Negligible concurrent marshaling at default configuration.** With the default queue limit of 1000 and handler execution times of 50ms–5s, the marshaling phase (~5-10ms of `json.RawMessage` copy-pass) represents 0.1–10% of handler time. At steady state, only ~1-10 requests are concurrently in the marshaling phase — far below the queue limit. The "fan-out" scenario requires setting the queue limit to 1-2, which is not the default and not a recommended production configuration.

2. **Relocating work ≠ eliminating it (meta-pattern #6).** The proposal extends the queue slot to cover marshaling but does not reduce any CPU or allocation work. Each request still allocates ~10-20MB for marshaling + envelope construction. Holding the slot longer means each slot is occupied for 5-10ms more, which at 1000 slots reduces peak throughput by ~0.1-0.2% — but saves nothing in per-request cost.

3. **Global HTTP backlog already bounds total concurrency.** Even if the per-method queue releases early, the global HTTP backlog limiter (default 5000) and HTTP duration limiter still wrap the entire `directBridge.ServeHTTP` call including marshaling. There is no "unbounded" fan-out path — total concurrent requests in any state are bounded by the global limit.

4. **Throughput impact is negative.** Holding the queue slot through the marshaling phase extends slot occupancy by ~5-10ms per request. Under high load near the queue limit, this means more requests are rejected or queued, reducing throughput without providing any per-request latency benefit.

### Lesson Learned

Queue-boundary and timeout-boundary placement hypotheses (this and condensed H007) both fail for the same fundamental reason: moving the accounting boundary doesn't eliminate any CPU work or allocation. For `getTransactions`, the response-side `json.Marshal` on a `json.RawMessage`-dominated struct is already an efficient copy-pass. The only viable network-layer performance targets are those that eliminate serialization passes entirely (e.g., streaming, zero-copy), not those that change where existing work is accounted for.
