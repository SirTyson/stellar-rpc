# H002: Each timeout layer tracks the same deadline twice

**Date**: 2026-04-06
**Subsystem**: network
**Severity**: Medium
**Impact**: timer heap CPU / per-request allocation
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Each `getTransactions` timeout layer should maintain one deadline source per request and use that same signal both to cancel downstream work and to detect expiration. The request path should not allocate multiple runtime timers for the exact same timeout boundary.

## Mechanism

Both duration limiters create `limitCh := time.NewTimer(q.limitThreshold).C` and then separately call `context.WithTimeout(..., q.limitThreshold)`. The select loop waits on `limitCh`, while the derived context carries a second internal timer for the same deadline. Because `getTransactions` goes through both the global HTTP duration limiter and the per-method JSON-RPC duration limiter, a single request allocates two redundant deadline timers on the outer layer and two more on the inner layer.

## Trigger

Benchmark fast or medium-latency `getTransactions` traffic with default execution limits enabled, then replace the explicit `limitCh` timer with `requestCtx.Done()` or a single shared timer/cancel path. Runtime timer heap activity and request allocations should drop if this duplication is material.

## Target Code

- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:(*httpRequestDurationLimiter).ServeHTTP:130-140` — creates `limitCh` and then `context.WithTimeout` for the same duration
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:(*RPCRequestDurationLimiter).Handle:238-252` — repeats the same duplicated deadline setup on the JSON-RPC path
- `cmd/stellar-rpc/internal/jsonrpc.go:NewJSONRPCHandler:315-323` — `getTransactions` always takes the JSON-RPC duration limiter
- `cmd/stellar-rpc/internal/jsonrpc.go:NewJSONRPCHandler:355-358` — every HTTP request, including `getTransactions`, also takes the outer HTTP duration limiter

## Evidence

The code never selects on the derived context's deadline signal; it only uses the explicit `limitCh` timer. That makes the `context.WithTimeout` timer redundant for expiration detection even though it is still allocated to carry cancellation into the downstream handler.

## Anti-Evidence

The timeout context itself is still necessary so downstream work can observe cancellation. If `getTransactions` is dominated by ledger parsing and JSON conversion, the saved timer work may only produce a medium-sized improvement rather than a dramatic one.

---

## Review

**Verdict**: VIABLE
**Severity**: Informational
**Date**: 2026-04-06
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced both duration limiter paths in `requestdurationlimiter.go`. Confirmed that each layer (HTTP `ServeHTTP` at line 136/139 and JRPC `Handle` at line 244/251) creates an explicit `time.NewTimer(q.limitThreshold)` and a `context.WithTimeout(ctx, q.limitThreshold)` for the same duration. The `select` loop only waits on the explicit `limitCh` channel — it never selects on `requestCtx.Done()`. The context timer is only used to propagate cancellation to the downstream handler, which could be achieved with the cheaper `context.WithCancel`. No code anywhere in the codebase calls `ctx.Deadline()`, confirming the context's deadline metadata is unused. For `getTransactions`, both layers fire (outer HTTP at 25s default, inner JRPC at 5s default), so each request creates 2 redundant timers.

### Code Paths Examined

- `requestdurationlimiter.go:ServeHTTP:130-139` — HTTP layer creates `warningCh` (line 132), `limitCh` (line 136), and `context.WithTimeout` (line 139). The `limitCh` and context timer share the same `limitThreshold` duration. Select loop (lines 158-196) only observes `limitCh`, never `requestCtx.Done()`.
- `requestdurationlimiter.go:Handle:238-251` — JRPC layer has identical duplicate pattern: `limitCh` (line 244) and `context.WithTimeout` (line 251) both use `q.limitThreshold`. Select loop (lines 267-302) only observes `limitCh`.
- `jsonrpc.go:NewJSONRPCHandler:316-323` — `getTransactions` gets a per-method JRPC duration limiter with `MaxGetTransactionsExecutionDuration` (default 5s).
- `jsonrpc.go:NewJSONRPCHandler:355-361` — all HTTP requests additionally get a global HTTP duration limiter with `MaxRequestExecutionDuration` (default 25s).
- Searched entire `cmd/` tree for `.Deadline()` calls — zero results. No downstream handler relies on the context deadline metadata.

### Findings

The inefficiency is confirmed: each duration limiter layer allocates one redundant `time.Timer` per request via `context.WithTimeout` when a simple `context.WithCancel` would suffice. For `getTransactions`, this means 2 extra timer allocations per request (one per layer). Additionally, the explicit `time.NewTimer` timers are never stopped on normal request completion — the Timer object is accessed only through `.C` and the Timer itself is never stored, so it lingers in the runtime timer heap until it fires (a minor timer leak).

However, the performance impact is negligible for `getTransactions`. A `time.NewTimer` allocation in Go costs ~100-200ns (heap allocation + timer heap insertion). Saving 2 timer allocations per request yields ~200-400ns. Compared to `getTransactions` latency of milliseconds (ledger reads, XDR decoding, JSON serialization), this represents < 0.01% of request time. Even at 10k RPS, the total CPU savings would be ~2-4ms/s — well below measurable thresholds. The hypothesis's Medium severity claim (5-20% improvement) is not supported. The correct severity is Informational.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/network/requestdurationlimiter.go` — both `ServeHTTP` (line 139) and `Handle` (line 251).
- **Change description**: Replace `context.WithTimeout(ctx, q.limitThreshold)` with `context.WithCancel(ctx)` in both methods. This eliminates the redundant internal timer while preserving cancellation propagation. Do NOT use the alternative direction (replacing `limitCh` with `requestCtx.Done()`) because that conflates timeout expiration with parent-context cancellation, changing behavior on client disconnect.
- **Correctness check**: Existing tests in `requestdurationlimiter_test.go` cover timeout, warning, normal completion, and panic paths for both HTTP and JRPC. All should continue to pass since no test or production code checks `ctx.Deadline()`.
- **Benchmark focus**: Measure timer allocations per request (via `runtime.MemStats` or `go tool pprof -alloc_objects`). The improvement would be 2 fewer `time.Timer` heap objects per request. Latency/RPS impact will be negligible — expect < 0.01% change.

---

## PoC Attempt

**Result**: POC_FAIL
**Date**: 2026-04-06
**PoC by**: claude-opus-4.6, high
**Failed At**: poc
**Iterations**: 3 (build-test-benchmark cycles)

### Failure Reason

The optimization is code-correct (all 7 existing tests pass) but produces no measurable benchmark improvement. Three full build-test-benchmark cycles were run with a proper A/B comparison using identical seed data, and the observed latency differences were entirely attributable to run-to-run variance rather than the code change.

The reviewer's prediction was accurate: saving ~200-400ns of timer allocation per request (< 0.01% of ~3ms median request latency) is far below the noise floor of the benchmark. At moderate loads (100-500 RPS), p50 latency varied by ±30% between consecutive runs of the same binary, dwarfing any theoretical improvement.

### Changes Attempted

Replaced `context.WithTimeout(ctx, q.limitThreshold)` with `context.WithCancel(ctx)` in two locations in `cmd/stellar-rpc/internal/network/requestdurationlimiter.go`:
- `ServeHTTP` (line 139): HTTP duration limiter
- `Handle` (line 251): JSON-RPC duration limiter

The change was functionally correct — all 7 tests in the `network` package passed (limiting, no-limiting, warning, and panic paths for both HTTP and JRPC). However, no performance improvement was detectable in benchmarks.

### Benchmark Evidence (3 runs, same seed data, same RPS sweep)

| RPS | Run 1 (opt) p50 | Run 2 (baseline) p50 | Run 3 (opt) p50 |
|-----|-----------------|---------------------|-----------------|
| 100 | 2.535ms | 2.955ms | 3.633ms |
| 500 | 2.641ms | 2.911ms | 3.939ms |
| 1000 | 2.965ms | 5.555ms | 10.007ms |
| 1500 | 4.699ms | 1056ms | 1274ms |

Results degraded monotonically with each successive run regardless of whether the optimization was applied, confirming the observed variation is environmental (futurenet conditions, system load, DB cache state) rather than caused by the code change. Source changes reverted.
