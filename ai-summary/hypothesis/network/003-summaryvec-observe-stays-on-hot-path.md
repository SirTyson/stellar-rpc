# H003: `SummaryVec` quantile tracking adds avoidable per-request CPU in the `getTransactions` wrapper

**Date**: 2026-04-06
**Subsystem**: network
**Severity**: Low
**Impact**: metrics CPU / allocation
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Latency instrumentation for `getTransactions` should record one cheap observation per request, ideally with negligible allocation and synchronization overhead relative to the method body. The network wrapper should not perform expensive quantile bookkeeping on every request if a lower-overhead metric type would provide equivalent operational value.

## Mechanism

`decorateHandlers` registers `json_rpc_request_duration_seconds` as a `prometheus.SummaryVec` with three quantile objectives and then does `requestMetric.With(label).Observe(...)` on every request. That path allocates a fresh `prometheus.Labels` map, performs label lookup, and updates the summary's quantile streams under synchronization for every `getTransactions` call. Replacing this with pre-bound observers and/or a histogram-based metric would remove wrapper CPU that is unrelated to ledger reads or response generation.

## Trigger

Benchmark `getTransactions` at sustained concurrency with metrics enabled, then compare against a build that switches this metric to `HistogramVec` or another cheaper observer with pre-bound labels. Measure wrapper CPU, allocations per request, and RPS.

## Target Code

- `cmd/stellar-rpc/internal/jsonrpc.go:decorateHandlers:74-80` — creates a `SummaryVec` with 0.5/0.9/0.99 quantile objectives
- `cmd/stellar-rpc/internal/jsonrpc.go:decorateHandlers:89-101` — allocates request labels and calls `requestMetric.With(label).Observe(duration.Seconds())` on every request

## Evidence

The observation happens for every `getTransactions` request regardless of success size, log level, or timeout settings. The label set is small and bounded, which means a cheaper pre-bound metric path is feasible without changing endpoint behavior.

## Anti-Evidence

There is only one observation per request, so the absolute cost is smaller than the duplicate response-marshaling issue above. If `getTransactions` latency is dominated by ledger decoding and JSON conversion, this wrapper-only optimization may stay below a 5% gain.
