# H013: Unregistered network backlog metrics are dead hot-path work but too small to optimize

**Date**: 2026-04-07
**Subsystem**: network
**Severity**: Low
**Impact**: dead metric atomic overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

If the network wrappers spend meaningful `getTransactions` time updating metrics, those metrics should either be exported or their removal should measurably improve throughput or latency. A viable finding needs the per-request metric work itself to be large enough to matter.

## Mechanism

I investigated whether the per-method backlog gauges and duration counters created in `NewJSONRPCHandler` are actually registered. They are not: unlike `requestMetric`, the backlog/duration metrics are constructed and then passed into the network wrappers without any matching `MetricsRegistry().MustRegister(...)` call. That means the hot path does perform real metric operations with no observability benefit, but the success-path work is mainly a few atomic `Inc`/`Dec` operations on the two backlog gauges.

## Trigger

Compare admitted `getTransactions` traffic before and after removing the unused backlog gauge updates or registering those metrics properly, focusing on wrapper-only CPU rather than end-to-end latency.

## Target Code

- `cmd/stellar-rpc/internal/jsonrpc.go:74-106` — `requestMetric` is created and explicitly registered
- `cmd/stellar-rpc/internal/jsonrpc.go:286-323` — per-method backlog gauge and duration counters are created for `getTransactions`, but no registration follows
- `cmd/stellar-rpc/internal/jsonrpc.go:332-361` — the global backlog gauge and global duration counters are also created without registration
- `cmd/stellar-rpc/internal/network/backlogQ.go:95-103` — admitted HTTP requests call `gauge.Inc()`/`Dec()`
- `cmd/stellar-rpc/internal/network/backlogQ.go:130-139` — admitted JRPC requests also call `gauge.Inc()`/`Dec()`

## Evidence

The registration contrast is explicit in the code: `requestMetric` is registered immediately, while the network-specific gauges/counters are not. Because the wrappers still receive those metric objects, every admitted `getTransactions` request performs hot-path gauge updates that currently cannot be scraped.

## Anti-Evidence

The success-path cost is just a couple of gauge atomics in the HTTP limiter and a couple more in the JRPC limiter. The unregistered duration counters are only touched on slow-request warning/timeout paths. That is operationally sloppy, but performance-wise it is far too small to justify a dedicated `getTransactions` optimization.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The unused metric objects are real, but their hot-path cost is only a handful of atomic gauge operations per request and cannot plausibly produce a measurable endpoint win.

### Lesson Learned

When a suspected overhead source is just Prometheus gauge increments, it is more likely to be an observability bug than a worthwhile `getTransactions` performance optimization.
