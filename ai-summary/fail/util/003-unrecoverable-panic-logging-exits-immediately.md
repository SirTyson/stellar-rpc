# H003: Unrecoverable Panic Logging Cannot Create Sustained Request Contention

**Date**: 2026-04-07
**Subsystem**: util
**Severity**: Informational
**Impact**: panic-path CPU / stderr I/O
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

If util panic handling were a meaningful `getTransactions` optimization target, the service would need to continue serving requests while paying repeated panic-recovery overhead. A viable optimization would reduce CPU or I/O consumed during steady-state request processing without changing failure semantics.

## Mechanism

I investigated whether `PanicGroup.recoverRoutine()` could steal enough CPU and stderr/log bandwidth to hurt concurrent `getTransactions` requests because it walks the formatted stack and writes each line to both the logger and `os.Stderr`. If recoverable background goroutines kept panicking under load, batching or truncating those writes could in theory reduce contention.

## Trigger

Force a panic in one of the production goroutines launched through `PanicGroup` while the server is handling `getTransactions` traffic.

## Target Code

- `cmd/stellar-rpc/internal/util/panicgroup.go:(*PanicGroup).recoverRoutine:63-89` — logs every stack line, increments metrics, optionally exits
- `cmd/stellar-rpc/internal/daemon/daemon.go:(*Daemon).Run:537-553` — wraps HTTP server goroutines in an unrecoverable panic group
- `cmd/stellar-rpc/internal/ingest/service.go:(*Service).Start:98-120` — wraps ingestion goroutine in an unrecoverable panic group

## Evidence

`recoverRoutine()` fans the stack trace out line-by-line to multiple sinks before handling the fatality policy, so the panic path is definitely heavier than a bare `recover()`.

## Anti-Evidence

Both production call sites use `NewUnrecoverablePanicGroup()`, which sets `exitProcessOnPanic` and terminates the process after the first recovered panic. That means there is no sustained period where repeated util panic logging can degrade ongoing `getTransactions` throughput; the process simply exits.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The live `PanicGroup` users are fatal-on-panic, so any panic ends the process instead of creating a long-running contention scenario that could be optimized.

### Lesson Learned

For util panic-handling ideas, confirm whether production call sites are recoverable or fatal; fatal wrappers are reliability behavior, not a request-path performance lever.
