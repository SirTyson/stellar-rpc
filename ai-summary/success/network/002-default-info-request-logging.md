# 002: Skip default `info`-level per-request JSON-RPC logging

**Date**: 2026-04-07
**Severity**: Low
**Impact**: logging I/O / lock contention
**Subsystem**: network
**Final review by**: gpt-5.4, high

## Summary

`getTransactions` was emitting two structured JSON-RPC `info` logs per request at the default log level: one on request start and one on request completion. In isolated baseline and optimized worktrees, gating those logs behind debug mode reduced `getTransactions` latency by 4.4% at the last valid zero-error load level (100 RPS), with a 6.45% p95 reduction but no improvement to the throughput ceiling.

## Root Cause

`decorateHandlers` unconditionally called `logRequest` and `logResponse`, and both functions emitted `logger.Info(...)` records. At `--log-level info`, every successful `getTransactions` call therefore paid for field assembly, logrus locking/formatting, and two synchronous writes even though this request tracing is observational rather than required for correctness.

## Reproduction

At the default `--log-level info`, every `getTransactions` request traverses `decorateHandlers`, executes `logRequest` before the handler body, and executes `logResponse` after the handler returns. That means the hot path performs two logrus `Info` writes before the HTTP layer finishes sending the response, so high-rate traffic spends extra time in logger formatting and output serialization.

## Affected Code

- `cmd/stellar-rpc/internal/jsonrpc.go:74-112` — `decorateHandlers` wraps every JSON-RPC method and previously invoked request/response logging on every call.
- `cmd/stellar-rpc/internal/jsonrpc.go:115-127` — `logRequest` built request-scoped fields and emitted an `Info` request-start log.
- `cmd/stellar-rpc/internal/jsonrpc.go:129-147` — `logResponse` built response-scoped fields and emitted an `Info` completion log.

## Optimization

- **Files modified**: `cmd/stellar-rpc/internal/jsonrpc.go` — gate `logRequest` and `logResponse` behind the precomputed debug-level flag and downgrade the per-request log lines to `Debug`.
- **How to verify**:
  1. Build: `make -j8 build-stellar-rpc`
  2. Run existing tests: `make go-test && cargo test`
  3. Benchmark: run `stellar-rpc-blaster generate --count 1000 --ledger-window <start,end>` and `stellar-rpc-blaster run` against isolated baseline and optimized builds with a getTransactions-only config

### Changes Made

The confirmed change reuses the existing `debugEnabled` flag passed into `decorateHandlers` and uses it to skip both `logRequest` and `logResponse` entirely unless debug logging is enabled. The log statements inside those helpers were also downgraded from `Info` to `Debug`, so enabling debug restores the detailed request tracing without paying the cost at the default `info` setting.

### Benchmark Results

These numbers are from an independent benchmark run by the final reviewer using `stellar-rpc-blaster`. I built isolated baseline and optimized worktrees that were identical except for `cmd/stellar-rpc/internal/jsonrpc.go`, ran the existing tests, then ran the required sweep. Under the objective's validity rule, 100 RPS was the highest zero-error level whose p50 did not jump more than 20% versus the previous step for either build.

| Metric | Before | After | Improvement |
|--------|--------|-------|-------------|
| p50 latency | 9.991 ms | 9.551 ms | 4.40% |
| p95 latency | 34.463 ms | 32.239 ms | 6.45% |
| p99 latency | 40.959 ms | 40.063 ms | 2.19% |
| Max RPS (0 errors) | 100 | 100 | 0% |
| Errors | 0 | 0 | — |

Relevant raw benchmark excerpts:

```text
baseline @ 100 RPS
total_requests=2749 success=2749 errors=0
p50=9.991ms p95=34.463ms p99=40.959ms p99.9=46.975ms

optimized @ 100 RPS
total_requests=2748 success=2748 errors=0
p50=9.551ms p95=32.239ms p99=40.063ms p99.9=44.127ms

baseline @ 200 RPS
total_requests=5497 success=5497 errors=0
p50=12.479ms (24.9% step-up vs 100 RPS)

optimized @ 200 RPS
total_requests=5493 success=5493 errors=0
p50=13.071ms (36.9% step-up vs 100 RPS)
```

## Expected vs Actual Behavior

- **Expected**: Default `info` logging should avoid per-request JSON-RPC tracing unless the operator has explicitly enabled debug-level diagnostics.
- **Actual**: The old code emitted request-start and request-finish `Info` logs for every JSON-RPC call, including `getTransactions`, adding two synchronous log writes to the hot path.

## Adversarial Review

1. Exercises claimed inefficiency: YES — the isolated diff only changes whether the JSON-RPC request/response log calls run at `info`.
2. Realistic preconditions: YES — default deployments use `--log-level info`, and baseline logs show these request/response lines on every JSON-RPC health probe while the optimized logs show none.
3. Inefficiency vs by-design: INEFFICIENCY — per-request tracing is diagnostic, not a correctness requirement, and debug mode still preserves it when requested.
4. Final severity: Low — the last valid zero-error level showed a 4.40% p50 reduction and a 6.45% p95 reduction, but the throughput ceiling remained 100 RPS.
5. In scope: YES — the change is in the `getTransactions` JSON-RPC call chain and does not depend on schema, third-party dependency, or Soroban preflight changes.
6. Benchmark methodology: CORRECT — I built clean baseline and optimized worktrees from the same `HEAD`, applied only the `jsonrpc.go` delta to the optimized tree, ran `make go-test && cargo test`, generated fresh seed data from the benchmarked server, and compared the same RPS ladder on the same hardware.
7. Alternative explanations: NONE PLAUSIBLE — the optimized server log removes the JSON-RPC `Info` lines entirely while the baseline server continues to emit them, and the latency win appears at the same valid load level with no other code difference between the benchmark trees.
8. Novelty: NOVEL

## Suggested Follow-Up

NONE
