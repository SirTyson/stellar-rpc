# H001: Queue-full getTransactions requests still allocate timeout machinery

**Date**: 2026-04-06
**Subsystem**: network
**Severity**: High
**Impact**: overload CPU / rejected-request RPS
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When the global HTTP backlog or the per-method `getTransactions` backlog is already full, the request should be rejected synchronously before the server allocates timeout state. Rejected overload traffic should not spawn extra goroutines, create timer channels, or allocate request-scoped timeout contexts that compete with admitted `getTransactions` work.

## Mechanism

`NewJSONRPCHandler` wraps both the global HTTP backlog limiter and the per-method JSON-RPC backlog limiter *inside* duration limiters. As a result, even requests that will be rejected immediately for queue saturation still run through `MakeHTTPRequestDurationLimiter` / `MakeJrpcRequestDurationLimiter`, which create timers, a timeout context, a completion channel, and a goroutine before reaching the backlog check. Under `getTransactions` floods above the configured queue limits, this turns the reject path into a relatively expensive path and can materially cut useful RPS.

## Trigger

Drive concurrent `getTransactions` traffic above either the global backlog limit or the per-method `getTransactions` backlog limit, keeping the service saturated so most requests are rejected. Compare CPU and rejected-request throughput before and after moving backlog admission ahead of the duration limiter stack.

## Target Code

- `cmd/stellar-rpc/internal/jsonrpc.go:NewJSONRPCHandler:291-323` — per-method `getTransactions` handler is wrapped as `durationLimiter(queueLimiter(handler))`
- `cmd/stellar-rpc/internal/jsonrpc.go:NewJSONRPCHandler:337-358` — global HTTP path is wrapped as `httpDurationLimiter(httpBacklogLimiter(bridge))`
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:(*httpRequestDurationLimiter).ServeHTTP:124-196` — allocates timeout state and goroutine before calling downstream
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:(*RPCRequestDurationLimiter).Handle:233-303` — same pattern on the JSON-RPC layer
- `cmd/stellar-rpc/internal/network/backlogQ.go:(*BacklogHTTPQLimiter).ServeHTTP:75-107` — actual global rejection point
- `cmd/stellar-rpc/internal/network/backlogQ.go:(*BacklogJrpcQLimiter).Handle:109-143` — actual per-method rejection point

## Evidence

Both duration limiters always create `warningCh`, `limitCh`, `requestCompleted`, and a timeout context, then spawn a goroutine that calls the downstream handler. The downstream handler is the backlog limiter, so queue-full requests do not short-circuit until after all of that setup has already happened.

## Anti-Evidence

Once the request reaches the backlog limiter, rejection is immediate and does avoid the expensive ledger scan inside `getTransactions`. If the server almost never saturates its queues, the optimization only helps overload scenarios rather than the steady-state healthy path.

---

## Review

**Verdict**: VIABLE
**Severity**: Medium
**Date**: 2026-04-06
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full wrapping order in `NewJSONRPCHandler` (jsonrpc.go:291-323, 337-360) and confirmed the hypothesis: both the per-method JRPC chain and the global HTTP chain wrap the backlog limiter *inside* the duration limiter. Every request—including those immediately rejected for queue saturation—pays the full duration limiter setup cost: 2 timer allocations, 1 buffered channel, 1 `context.WithTimeout`, 1 goroutine spawn, and (at the HTTP level) a `bufferedResponseWriter` with header-map clone. The reject path inside the goroutine is a single atomic compare-and-return, making the duration limiter overhead dominate the cost of rejection. Additionally, the timers are created via `time.NewTimer().C` without storing the `Timer` handle, so they cannot be stopped and leak into the runtime timer heap until they fire (typically seconds later).

### Code Paths Examined

- `cmd/stellar-rpc/internal/jsonrpc.go:NewJSONRPCHandler:291-323` — confirmed wrapping order: `durationLimiter(queueLimiter(handler))` for per-method handlers including getTransactions. The `durationLimiter.Handle` is the entry point, `queueLimiter.Handle` is its downstream.
- `cmd/stellar-rpc/internal/jsonrpc.go:NewJSONRPCHandler:337-360` — confirmed wrapping order: `httpDurationLimiter(httpBacklogLimiter(bridge))` for the global HTTP layer. Same inside-out pattern.
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:RPCRequestDurationLimiter.Handle:233-303` — confirmed: allocates 2 timers (lines 238-244), 1 channel (line 250), 1 context.WithTimeout (line 251), spawns goroutine (line 254) before calling downstream (line 262). All of this executes before the backlog limiter's atomic admission check.
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:httpRequestDurationLimiter.ServeHTTP:124-196` — confirmed: same allocation pattern plus `makeBufferedResponseWriter` (line 142) which clones the header map (line 80).
- `cmd/stellar-rpc/internal/network/backlogQ.go:BacklogJrpcQLimiter.Handle:109-143` — the actual rejection is a single `atomic.AddUint64` + compare + `atomic.AddUint64` decrement (lines 115-117), taking ~nanoseconds. Confirmed the dramatic asymmetry: the setup cost in the duration limiter dwarfs the actual rejection cost.

### Findings

**The inefficiency is real and confirmed.** Under overload where most requests are rejected:

1. **Per-method JRPC layer**: Every getTransactions request enters `RPCRequestDurationLimiter.Handle`, which allocates 2 timers, 1 channel, 1 context, and spawns a goroutine. Inside the goroutine, `BacklogJrpcQLimiter.Handle` does a ~10ns atomic check and rejects. The goroutine completes and sends the error on the channel. The select loop picks it up and returns. Net waste per rejection: ~2-5μs of allocations + goroutine scheduling.

2. **Global HTTP layer**: Same pattern with additional `bufferedResponseWriter` allocation.

3. **Timer leak**: `time.NewTimer(q.warningThreshold).C` and `time.NewTimer(q.limitThreshold).C` discard the Timer handle. These timers sit in the runtime timer heap for the full warning/limit duration (typically 10s-30s) before firing into a channel nobody reads. Under 1000 rejections/sec, this accumulates 10k-30k leaked timers in the heap.

4. **Impact on admitted requests**: The goroutine churn and timer heap bloat from rejected requests compete for CPU and scheduler resources with the goroutines handling admitted getTransactions requests.

**Severity downgraded to Medium**: While the waste per rejection is significant relative to the rejection cost itself (orders of magnitude), the absolute CPU time per rejection (~2-5μs) means this likely produces 5-20% improvement under sustained overload rather than >20%. The timer leak is the strongest amplifier—at high rejection rates, the growing timer heap adds GC and scheduler pressure that could push impact toward the higher end.

**Correctness note**: Swapping to `queueLimiter(durationLimiter(handler))` changes the semantics slightly for the timeout case: when a duration limiter times out, the queue slot is freed immediately (queue limiter defer runs), but the handler goroutine may still be running briefly. In the current order, the queue slot stays occupied until the handler actually completes. This means a brief window where concurrent handlers could exceed the queue limit by a small amount. This is acceptable because: (a) context cancellation causes rapid handler exit, (b) it only affects the timeout path (not the common rejection path being optimized), and (c) the existing NoLimit pass-through already allows unlimited concurrency when the sentinel is set.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/jsonrpc.go:NewJSONRPCHandler` — swap the wrapping order at two locations:
  1. Lines 291-323: Change to `durationLimiter(handler)` first, then `queueLimiter(durationLimiter)` — i.e., `queueLimiter := MakeJrpcBacklogQueueLimiter(durationLimiter.Handle, ...)` and expose `queueLimiter.Handle`.
  2. Lines 337-360: Change to `MakeHTTPRequestDurationLimiter(bridge, ...)` first, then `MakeHTTPBacklogQueueLimiter(durationLimitedBridge, ...)`.
- **Change description**: Swap the wrapping order so backlog admission (cheap atomic check) happens before duration limiter setup (goroutine + timers + channel + context). Rejected requests short-circuit with only an atomic operation instead of full timeout machinery.
- **Correctness check**: Existing tests in `cmd/stellar-rpc/internal/network/backlogQ_test.go` and `cmd/stellar-rpc/internal/network/requestdurationlimiter_test.go` cover both limiters independently. Integration tests that drive traffic above backlog limits should still pass. Verify that the queue limiter's pending count still correctly tracks in-flight requests.
- **Benchmark focus**: Measure rejected-request throughput (rejections/sec) and CPU utilization under overload. Set backlog limit to a small number (e.g., 5), drive 100+ concurrent getTransactions requests, and compare: (1) rejected request throughput, (2) p99 latency of admitted requests, (3) goroutine count, (4) timer heap size. Expected improvement: 2-5x increase in rejection throughput; 5-20% reduction in admitted request latency under sustained overload.
