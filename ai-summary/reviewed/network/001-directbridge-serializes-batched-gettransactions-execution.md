# H001: `directBridge` serializes batched `getTransactions` execution

**Date**: 2026-04-07
**Subsystem**: network
**Severity**: High
**Impact**: batch request parallelism / throughput
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

A JSON-RPC batch containing multiple independent `getTransactions` calls should preserve the server's normal per-member parallelism, so total batch latency is bounded closer to the slowest member plus response assembly, not the sum of all member runtimes. Independent batched calls should still be able to use the existing per-method backlog and duration limiters concurrently.

## Mechanism

`directBridge.serveInternal` currently iterates over parsed requests in a simple `for` loop and calls each assigned handler inline before moving to the next member. Upstream `jrpc2.Server.dispatchLocked`, by contrast, fans batch members out with goroutines and only waits at the end, so the current bridge turns a batch of N expensive `getTransactions` calls into serialized work under one HTTP request. For batch clients, that directly inflates latency and lowers effective RPS by roughly the batch width whenever the members are independently runnable.

## Trigger

Send one HTTP JSON-RPC batch containing 4-16 `getTransactions` requests with `xdrFormat=json` and `pagination.limit=200`, pointed at dense historical ledgers. Compare baseline against a version of `directBridge` that dispatches batch members concurrently into indexed result slots and only joins responses after a `WaitGroup`, preserving input order in the final batch reply.

## Target Code

- `cmd/stellar-rpc/internal/directbridge.go:69-123` — batch members are executed one-by-one inside the `for _, parsed := range jreqs` loop
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/server.go:241-279` — upstream batch dispatch fans requests out concurrently and waits only before delivery
- `cmd/stellar-rpc/internal/jsonrpc.go:318-330` — each `getTransactions` handler already carries its own backlog and duration wrappers, so concurrent batch dispatch can still reuse existing admission control

## Evidence

The live bridge code has no goroutines, `WaitGroup`, or indexed response slots in the batch path; every batch member finishes handler execution and response marshaling before the next begins. The upstream server code explicitly does the opposite for batched requests, which means the current direct bridge has given up a real source of parallelism that large batched `getTransactions` calls used to have.

## Anti-Evidence

This does nothing for the dominant single-request traffic shape, and parallel batch execution will raise instantaneous CPU and heap demand for clients that send very wide batches. The implementation also has to preserve upstream JSON-RPC batch semantics, especially response ordering and notification behavior.

---

## Review

**Verdict**: VIABLE
**Severity**: Medium
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated. Related reviewed hypothesis 003 targets batch response *accumulation/recopying* (memory/copy amplification), not execution parallelism. Fail summary contains no batch parallelism investigations.

### Trace Summary

Traced `directBridge.serveInternal` (directbridge.go:58-126) and confirmed the batch loop at line 72 processes each member sequentially: `Assign` → `ToRequest` → handler call → `json.Marshal` → envelope construction, all synchronous before the next iteration. Verified the upstream `jrpc2.Server.dispatchLocked` (server.go:241-279) dispatches batch members concurrently via goroutines + `sync.WaitGroup`. Confirmed the per-method handler stack (jsonrpc.go:315-330) wraps each handler with independent backlog and duration limiters, making concurrent dispatch safe. `handler.Map.Assign` is a simple map read (safe for concurrent access), and `ToRequest()` allocates a fresh `*Request` per call (no shared mutable state).

### Code Paths Examined

- `cmd/stellar-rpc/internal/directbridge.go:58-126` — `serveInternal` processes batch members sequentially in a `for` range loop; each iteration calls `b.assigner.Assign`, `parsed.ToRequest`, handler `h()`, `json.Marshal(result)`, and `directBridgeSuccessResponse` before advancing
- `cmd/stellar-rpc/internal/directbridge.go:72-113` — the sequential loop: no goroutines, no WaitGroup, no indexed result slots; `results` grows by `append` within one goroutine
- `cmd/stellar-rpc/internal/jsonrpc.go:315-330` — per-method handler stack: `underlyingHandler` → `durationLimiter.Handle` → `queueLimiter.Handle` → `decorateHandlers` wrapper; each layer is per-method and concurrency-safe
- `cmd/stellar-rpc/internal/jsonrpc.go:76-119` — `decorateHandlers` wraps each endpoint with metrics and logging; uses pre-bound `okObserver` and `requestMetric.WithLabelValues` — both are goroutine-safe Prometheus types
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/server.go:241-279` — upstream `dispatchLocked` fans N-1 batch members into goroutines via `wg.Add(1)` + `go func()`, runs last member inline, then `wg.Wait()`
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/handler/handler.go:24` — `handler.Map` is `map[string]jrpc2.Handler`; `Assign` is a simple map index — safe for concurrent reads
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/json.go:ParseRequests,ToRequest` — `ParseRequests` returns independent `*ParsedRequest` structs; `ToRequest` allocates a new `*Request` each call — no shared state

### Findings

1. **The inefficiency is real.** `serveInternal` processes batch members strictly sequentially. For a batch of N `getTransactions` calls each taking T ms, total batch latency is N×T instead of ≈T. For N=4 with T=50ms, that's 200ms → ~50ms (75% reduction). For N=16, 800ms → ~50ms (94% reduction).

2. **Concurrent dispatch is safe.** Each handler invocation operates on an independent `*Request`, goes through its own backlog/duration limiter stack, and returns an independent `(any, error)`. `handler.Map.Assign` is a concurrent-safe map read. Prometheus observers (`okObserver`, `requestMetric`) are goroutine-safe. The only coordination needed is indexed result slots and a WaitGroup.

3. **The upstream library already does this.** `jrpc2.Server.dispatchLocked` uses the exact pattern the hypothesis proposes (goroutines + WaitGroup, last member inline). The directBridge lost this parallelism when it replaced the jrpc2 server with direct handler dispatch.

4. **Correctness constraints are manageable.** Response ordering is preserved by using pre-allocated indexed result slots (`results[i]` instead of `append`). Notifications (no response expected) can be dispatched concurrently and their results discarded. Error responses are per-member and independent. The shared `req.Context()` is read-only and safe to pass to multiple goroutines.

5. **Severity downgrade from High to Medium.** The optimization is transformative for batch clients (>75% latency reduction for N≥4), but the dominant traffic shape is single requests where this is a no-op. The actual production impact depends on the proportion of batch traffic, which is not established. Per the severity criteria (>20% for High requires impact on getTransactions broadly), Medium is appropriate given the single-request dominance.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/directbridge.go:69-126` — replace the sequential `for` loop with concurrent dispatch for batch requests (len(jreqs) > 1)
- **Change description**: Pre-allocate `results = make([]json.RawMessage, len(jreqs))`. For batch requests, dispatch each member in a goroutine (using a `sync.WaitGroup`), writing to `results[i]` by index. Run the last member inline (matching upstream pattern). For single requests, keep the current synchronous path unchanged. Each goroutine handles its own error marshaling and envelope construction so the result slot always gets a valid JSON-RPC response.
- **Correctness check**: Existing tests for batch dispatch in `directbridge_test.go` (if present) or `jsonrpc_test.go`; also the jrpc2 test suite for batch semantics. Run `make go-test` to verify no regressions.
- **Benchmark focus**: Send batch requests with N=4,8,16 `getTransactions` calls with `xdrFormat=json` and `pagination.limit=200`. Measure total batch latency (wall clock from HTTP request to response). Expect ~N:1 latency improvement compared to baseline. Also verify that single-request latency is unchanged (regression check).
