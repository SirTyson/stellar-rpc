# H002: Default `info`-level per-request logging can throttle `getTransactions` throughput

**Date**: 2026-04-06
**Subsystem**: network
**Severity**: Medium
**Impact**: logging I/O / lock contention
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

The default `getTransactions` hot path should avoid synchronous per-request log I/O unless the operator has explicitly opted into request tracing. High-volume methods should not emit two structured log entries by default if that logging can compete with request CPU and response delivery.

## Mechanism

`decorateHandlers` always calls `logRequest()` and `logResponse()`, and both emit `logger.Info(...)` records. Because the default `--log-level` is `info`, every successful `getTransactions` call performs request-ID generation, field assembly, formatter work, logger locking, and two writes to the configured sink even before considering any debug-only fields. In containerized deployments where stdout/stderr is harvested by Docker, journald, or a sidecar, this per-request logging can become a throughput ceiling for high-rate `getTransactions` traffic.

## Trigger

Benchmark high-concurrency `getTransactions` traffic with the default `--log-level info`, then compare against `warn` level or a version that samples/downgrades the request start/finish logs for high-volume methods. Watch CPU, syscall time, and RPS.

## Target Code

- `cmd/stellar-rpc/internal/jsonrpc.go:decorateHandlers:83-103` — wraps every JSON-RPC method, including `getTransactions`, with request/response logging
- `cmd/stellar-rpc/internal/jsonrpc.go:logRequest:110-121` — emits an `Info` log for request start and allocates request-scoped logging fields
- `cmd/stellar-rpc/internal/jsonrpc.go:logResponse:124-141` — emits an `Info` log for request completion and formats duration/status fields
- `cmd/stellar-rpc/internal/config/options.go:log-level option:140-143` — default server log level is `info`

## Evidence

The logging is on the success path for every request, not just slow paths or errors. Unlike optional metrics or timeout machinery, this work is enabled by the default configuration, so a stock deployment pays it immediately under any `getTransactions` load test.

## Anti-Evidence

Operators can already mitigate the issue by running at `warn` or `error`, so the impact depends on deployment configuration. If request rate is low or logs are written to a fast in-memory sink, the improvement may be smaller than in typical containerized production setups.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-06
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated (H001 covers the `json.Marshal` waste in `logResponse`, but H002 targets the two Info-level write operations themselves and their mutex/allocation/I/O overhead, which is a distinct cost)

### Trace Summary

Every JSON-RPC method call passes through the `decorateHandlers` closure (jsonrpc.go:83-104), which calls `logRequest()` at line 85 and `logResponse()` at line 102. Both functions create new logrus entries via `WithFields()` and emit `Info()` calls. Each `Info()` invocation in logrus v1.9.4 acquires the global logger mutex three times (once in `log()` to read config, once in `fireHooks()` to copy the hooks map, once in `write()` for formatting+I/O), allocates a `Dup()` of the entry with a full field-map copy, and performs a synchronous write to the output sink. Two Info writes per request means six mutex acquisitions, four heap allocations (two `WithFields` + two `Dup`), and two I/O syscalls on the hot path.

### Code Paths Examined

- `cmd/stellar-rpc/internal/jsonrpc.go:decorateHandlers:82-108` — wraps all handlers; line 84 calls `middleware.NextRequestID()` (atomic, cheap), line 85 calls `logRequest()`, line 102 calls `logResponse()`
- `cmd/stellar-rpc/internal/jsonrpc.go:logRequest:110-122` — `WithFields` with 4 fields (line 111-116) allocates new Entry + map via logrus `WithFields`; `Info()` at line 117 triggers the full log pipeline; `WithField("params", req.ParamString())` at line 120 allocates another Entry+map even at Info level (wasted since `Debug()` at line 121 returns early)
- `cmd/stellar-rpc/internal/jsonrpc.go:logResponse:124-142` — `WithFields` with 5 fields (line 125-131) allocates new Entry + map; `Info()` at line 132 triggers the full log pipeline
- `sirupsen/logrus@v1.9.4/entry.go:WithFields:128-155` — allocates `make(Fields, len(entry.Data)+len(fields))`, copies all existing fields, uses `reflect.TypeOf()` on each new field value
- `sirupsen/logrus@v1.9.4/entry.go:log:224-265` — calls `Dup()` (line 227, allocates Entry+map copy), acquires `Logger.mu.Lock()` at line 236, calls `fireHooks()` which acquires `Logger.mu.Lock()` again at line 276, calls `write()` which acquires `Logger.mu.Lock()` a third time at line 290 and holds it through `Formatter.Format()` + `Logger.Out.Write()`
- `cmd/stellar-rpc/internal/config/options.go:140-143` — default log level is `logrus.InfoLevel`

### Findings

The inefficiency is confirmed: every `getTransactions` request at the default Info level pays for two full logrus write cycles. The per-request cost breakdown:

1. **Allocations**: `WithFields` in `logRequest` (4 fields → 1 Entry+map), `WithFields` in `logResponse` (5 fields → 1 Entry+map), `Dup()` inside each `Info()` call (2 more Entry+map copies), plus the wasted `WithField("params", ...)` allocation in `logRequest` even though `Debug()` discards it. Total: ~5 heap allocations per request for logging alone.

2. **Mutex contention**: Each `Info()` call acquires `Logger.mu` three times — in `log()`, `fireHooks()`, and `write()`. Two Info calls = 6 mutex acquisitions per request on the same global mutex shared by ALL RPC methods. Under high concurrency, this serialization point limits parallelism.

3. **I/O**: Two synchronous writes to the log output per request. In containerized environments (Docker json-file driver, journald), each write involves a pipe write syscall and downstream processing.

4. **Formatting**: logrus `TextFormatter` or `JSONFormatter` iterates all fields per entry, building formatted output strings. This is CPU work inside the mutex hold in `write()`.

Estimated overhead: ~5–90μs per request depending on concurrency level and I/O backend. At typical `getTransactions` latencies of 1–10ms, this represents <5% of request time. The impact increases at very high concurrency due to mutex contention scaling across all RPC methods sharing the same logger.

This is distinct from H001 (which covers the `json.Marshal(response)` waste in `logResponse`). Both findings target the same `logResponse` function but identify different costs. Fixing H002 (downgrading or sampling the Info writes) would also eliminate the allocation overhead but would NOT fix the `json.Marshal` waste unless the entire logging block is restructured.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/jsonrpc.go` — functions `logRequest` (lines 110-122) and `logResponse` (lines 124-142), and the calls to them in `decorateHandlers` (lines 85, 102)
- **Change description**: Downgrade both `logger.Info(...)` calls to `logger.Debug(...)` in `logRequest` and `logResponse`. Alternatively, gate the entire `logRequest`/`logResponse` invocation behind a logger level check (e.g., `if logger.entry.Logger.IsLevelEnabled(logrus.DebugLevel)`) to skip all field assembly and allocation when not needed. A third option is to keep the Info writes but use sampling (e.g., log every Nth request) for high-volume methods.
- **Correctness check**: Existing tests in `cmd/stellar-rpc/internal/` that exercise JSON-RPC handlers should still pass since logging is observational. Check that no test asserts on Info-level log output from request/response logging.
- **Benchmark focus**: Measure RPS and p99 latency of `getTransactions` under high concurrency (100+ goroutines) at `--log-level info`. Compare baseline (two Info writes) vs. modified (Debug-only or gated writes). Expect <5% RPS improvement from this change alone. For maximum effect, combine with H001's `json.Marshal` fix.
