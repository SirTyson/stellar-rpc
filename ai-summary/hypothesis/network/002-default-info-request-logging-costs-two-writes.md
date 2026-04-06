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
