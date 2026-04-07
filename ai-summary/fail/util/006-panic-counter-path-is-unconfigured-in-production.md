# H006: Panic Counter Increments Never Run in Production getTransactions

**Date**: 2026-04-07
**Subsystem**: util
**Severity**: Informational
**Impact**: Prometheus contention / panic-path metrics cost
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

For a util metrics optimization to matter, `getTransactions` or its supporting goroutines would need to pay metric-update overhead during live traffic. If `PanicGroup.Counter()` configured a shared Prometheus counter on production paths, repeated `Inc()` calls could in principle add lock contention or cache-line bouncing during failure bursts.

## Mechanism

I investigated whether the `panicsCounter` field is wired into any production `PanicGroup` instances involved with `getTransactions`. Because `recoverRoutine()` increments the counter after formatting the panic stack, a configured counter would add extra synchronization work whenever the recovery path runs.

## Trigger

Inspect production `PanicGroup` construction for the HTTP server and ingestion service and check whether any code calls `Counter()` before serving `getTransactions`.

## Target Code

- `cmd/stellar-rpc/internal/util/panicgroup.go:(*PanicGroup).Counter:46-53` — configures the Prometheus counter on a copied panic group
- `cmd/stellar-rpc/internal/util/panicgroup.go:(*PanicGroup).recoverRoutine:63-85` — increments `panicsCounter` when configured
- `cmd/stellar-rpc/internal/daemon/daemon.go:(*Daemon).Run:537-553` — production setup calls `Log()` but not `Counter()`
- `cmd/stellar-rpc/internal/ingest/service.go:(*Service).Start:98-120` — production setup calls `Log()` but not `Counter()`

## Evidence

The counter hook is real: `recoverRoutine()` conditionally executes `pg.panicsCounter.Inc()`, so a wired counter would add extra metric work to the panic path.

## Anti-Evidence

Production setup never calls `Counter()`. The only production `PanicGroup` instances are created in daemon and ingestion startup, and both configure logging only. That leaves `panicsCounter` nil everywhere outside tests, so the increment path never runs.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The panic-counter path is unconfigured in production, so there is no metric-update overhead to remove from `getTransactions`.

### Lesson Learned

Before optimizing optional instrumentation hooks, verify that production actually enables them; disabled hooks are dead code, not hot-path waste.
