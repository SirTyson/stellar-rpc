# H001: Hidden jrpc2 semaphore caps `getTransactions` far below configured queue limits

**Date**: 2026-04-07
**Subsystem**: network
**Severity**: High
**Impact**: hidden bridge concurrency ceiling / throughput loss
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

`getTransactions` concurrency should be governed by the explicit backlog controls exposed in stellar-rpc configuration and by the actual downstream capacity of the method. When operators allow hundreds or thousands of outstanding requests, the in-process JSON-RPC bridge should not silently cap active handler execution at `runtime.NumCPU()` before those configured limiters even come into play.

## Mechanism

`NewJSONRPCHandler` passes a `jrpc2.ServerOptions` containing only a debug logger into `jhttp.NewBridge`, leaving `ServerOptions.Concurrency` unset. In jrpc2, an unset concurrency value becomes `runtime.NumCPU()`, and `Server.invoke` acquires a weighted semaphore before every handler call. That means `getTransactions` traffic can pile up behind a hidden bridge semaphore even when `request-backlog-get-transactions-queue-limit` is 1000 and the global backlog is 5000, turning bridge-local queueing into an undocumented throughput ceiling.

## Trigger

Run `getTransactions` with request concurrency well above the host CPU count but still below the configured backlog limits (for example, 64-256 concurrent calls on an 8-16 core host). Compare active handler count, queue wait time, and RPS before and after setting the bridge server concurrency explicitly (or making it configurable) instead of relying on the default `runtime.NumCPU()` cap.

## Target Code

- `cmd/stellar-rpc/internal/jsonrpc.go:158-162` — bridge server options are created with only `Logger`, so `Concurrency` is left at the jrpc2 default
- `cmd/stellar-rpc/internal/jsonrpc.go:325-329` — `jhttp.NewBridge(...)` installs that defaulted server into the HTTP path used by `getTransactions`
- `cmd/stellar-rpc/internal/config/options.go:431-435` — global backlog default is 5000 outstanding requests
- `cmd/stellar-rpc/internal/config/options.go:487-491` — `getTransactions` backlog default is 1000 outstanding requests
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/opts.go:37-40` — `ServerOptions.Concurrency` is the handler parallelism knob
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/opts.go:62-67` — unset or `<1` concurrency resolves to `runtime.NumCPU()`
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/server.go:100-105` — `NewServer` builds `sem := semaphore.NewWeighted(opts.concurrency())`
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/server.go:379-384` — every request acquires that semaphore in `invoke` before calling the handler

## Evidence

The configured request backlog limits are three orders of magnitude larger than the bridge's implicit default concurrency, but the bridge never exposes or aligns that cap. `getTransactions` is a good candidate for benefiting from a higher in-flight count because it now batches ledger reads and spends meaningful time in DB access, XDR parsing, JSON conversion, and FFI-heavy work rather than pure CPU-only computation.

## Anti-Evidence

If the workload is already purely CPU-bound on the test host, raising concurrency above `runtime.NumCPU()` may not help and can even worsen cache pressure. The likely fix is not "set this to 1000 blindly" but "make bridge concurrency explicit and tune it alongside the existing backlog controls."

---

## Review

**Verdict**: VIABLE
**Severity**: Medium
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full request path from HTTP ingress through the jhttp bridge into the jrpc2 server's dispatch loop. Confirmed that `NewJSONRPCHandler` at `jsonrpc.go:158-162` constructs `ServerOptions` with only a `Logger` field, leaving `Concurrency` at the int zero value. The bridge (`jhttp.NewBridge` at line 325) passes these options to `server.NewLocal`, which calls `jrpc2.NewServer`. In `NewServer` (server.go:102), `opts.concurrency()` returns `int64(runtime.NumCPU())` for any value < 1, and a `semaphore.NewWeighted` is constructed with that cap. Every handler invocation goes through `invoke` (server.go:379-384), which calls `sem.Acquire(ctx, 1)` — blocking if the cap is reached — before executing the handler. The bridge uses unbuffered `channel.Direct()` channels and dispatches each request batch in a new goroutine, so requests do run concurrently, but the semaphore is the hard ceiling. The per-method backlog limiters and duration limiters all sit *inside* this semaphore gate, meaning the configured backlog limits (1000 for getTransactions, 5000 global) are unreachable — the bridge caps active handlers at `runtime.NumCPU()` regardless.

### Code Paths Examined

- `cmd/stellar-rpc/internal/jsonrpc.go:158-162` — `bridgeOptions` created with `ServerOptions{Logger: ...}`, `Concurrency` is zero (confirmed: no other assignment to `Concurrency` exists in the codebase via grep)
- `cmd/stellar-rpc/internal/jsonrpc.go:325-329` — `jhttp.NewBridge(decorateHandlers(..., handlersMap), &bridgeOptions)` passes the zero-concurrency options through
- `jhttp/bridge.go:189-206` — `NewBridge` creates `server.NewLocal(mux, &LocalOptions{Server: opts.Server})` with the unmodified options
- `server/local.go:26-35` — `NewLocal` calls `jrpc2.NewServer(assigner, opts.Server).Start(spipe)` and `jrpc2.NewClient(cpipe, opts.Client)` on a `channel.Direct()` pair
- `channel/channel.go:111-117` — `Direct()` creates unbuffered `chan []byte` pair for in-memory client-server communication
- `opts.go:62-67` — `concurrency()` returns `int64(runtime.NumCPU())` when `s.Concurrency < 1`
- `server.go:96-110` — `NewServer` constructs `sem: semaphore.NewWeighted(opts.concurrency())`, hard-coding the cap for the lifetime of the server
- `server.go:170-182` — `serve()` dequeues request batches and spawns goroutines for dispatch, allowing concurrent in-flight requests
- `server.go:241-274` — `dispatchLocked` spawns goroutines for concurrent tasks in a batch, each calling `invoke`
- `server.go:379-384` — `invoke` calls `s.sem.Acquire(ctx, 1)` before `h(ctx, req)` — this is the throttle point

### Findings

The inefficiency is confirmed and mechanically real:

1. **The cap exists**: `runtime.NumCPU()` (typically 8-16 on production hosts) is the maximum number of concurrent handler invocations across ALL JSON-RPC methods sharing this single bridge server.

2. **It's in the hot path**: Every `getTransactions` request must pass through `invoke` → `sem.Acquire`. With concurrency above `NumCPU()`, requests block at the semaphore even though they've already been admitted by the backlog limiter.

3. **No existing mitigation**: There is no configuration option, pool, or cache that bypasses this semaphore. The `Concurrency` field is never set anywhere in stellar-rpc (confirmed via codebase-wide grep).

4. **Mixed workload profile**: `getTransactions` performs DB reads (I/O wait), XDR parsing (CPU), JSON encoding (CPU), and FFI calls (Cgo overhead with goroutine scheduling). This mixed profile means allowing more than `NumCPU()` concurrent handlers can improve throughput — while some handlers wait on I/O, others can use CPU cycles.

5. **Impact estimate**: On an 8-core host, the cap is 8 concurrent handlers. If `getTransactions` spends even 30-40% of wall time in I/O (DB reads, Cgo transitions), raising concurrency to 16-32 could yield 10-20% throughput improvement by overlapping I/O and CPU work. The exact gain depends on the I/O-to-CPU ratio and whether the DB layer can serve additional concurrent reads.

**Severity downgraded to Medium**: The improvement is real and likely in the 5-20% range for mixed workloads. Achieving >20% (High) would require the workload to be heavily I/O-bound, which needs benchmarking to confirm. The fix is also straightforward — a one-line change to set `Concurrency` explicitly.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/jsonrpc.go:158-162` — set `Concurrency` in the `jrpc2.ServerOptions` struct
- **Change description**: Add `Concurrency: N` (where N is e.g., `4 * runtime.NumCPU()` or a new config option) to the `ServerOptions` in `bridgeOptions`. A simple first test: `Concurrency: 64` on an 8-core host.
- **Correctness check**: The handlers already have their own concurrency controls via `BacklogJrpcQLimiter` (atomic counter) and `RPCRequestDurationLimiter` (goroutine + timeout). Raising the bridge semaphore does not bypass these safety layers. The `bufferedResponseWriter` and channel-based communication in the duration limiter are goroutine-safe by design.
- **Benchmark focus**: Measure `getTransactions` RPS and p50/p99 latency at concurrency levels of 32, 64, 128, 256 on an 8-16 core host. Compare against baseline (NumCPU cap). Expect 5-20% RPS improvement at concurrency levels well above NumCPU. Monitor CPU utilization to verify the workload has an I/O component that benefits from additional concurrency.
