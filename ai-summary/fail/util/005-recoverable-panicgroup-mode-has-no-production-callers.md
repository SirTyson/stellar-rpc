# H005: Recoverable PanicGroup Mode Is Dead Code for getTransactions

**Date**: 2026-04-07
**Subsystem**: util
**Severity**: Informational
**Impact**: panic recovery CPU / log I/O
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

A viable util optimization for `getTransactions` could target recoverable panic handling only if production request-serving code used the recoverable mode while continuing to serve traffic after a panic. In that scenario, reducing stack formatting or log fan-out could lower the cost of repeated recovery events.

## Mechanism

I investigated whether `NewRecoverablePanicGroup()` protects any production goroutines that remain live while `getTransactions` traffic continues. If recoverable groups were used around request-adjacent work, their panic-handling cost might degrade latency or tail behavior under failure.

## Trigger

Search the production code paths used to start the daemon and serve `getTransactions`, then identify whether any of them construct `NewRecoverablePanicGroup()`.

## Target Code

- `cmd/stellar-rpc/internal/util/panicgroup.go:23-35` — defines the recoverable and unrecoverable constructors
- `cmd/stellar-rpc/internal/daemon/daemon.go:(*Daemon).Run:537-553` — constructs an unrecoverable panic group for the HTTP servers
- `cmd/stellar-rpc/internal/ingest/service.go:(*Service).Start:98-120` — constructs an unrecoverable panic group for ingestion startup

## Evidence

The recoverable constructor exists specifically to keep the process alive after a goroutine panic, so it would be the only viable place for an optimization based on repeated panic recovery overhead.

## Anti-Evidence

Repository-wide search found no production callers of `NewRecoverablePanicGroup()`. The only live call sites use `NewUnrecoverablePanicGroup()`, which exits the process on the first recovered panic instead of continuing to serve `getTransactions`.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The recoverable PanicGroup mode is unused in production, so any optimization to its steady-state recovery behavior cannot improve `getTransactions`.

### Lesson Learned

Unused recovery modes are not optimization opportunities; first prove the mode is instantiated on live request-serving paths before analyzing its internals.
