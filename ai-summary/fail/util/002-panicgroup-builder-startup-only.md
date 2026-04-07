# H002: PanicGroup Builder Allocations Are Startup-Only

**Date**: 2026-04-07
**Subsystem**: util
**Severity**: Informational
**Impact**: startup allocation / no request-path effect
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

A util optimization for `getTransactions` should eliminate work that occurs while requests are being served. One-time configuration work during daemon startup should have no measurable effect on request latency once the process is already running.

## Mechanism

I investigated whether the copy-on-write builder methods `PanicGroup.Log()` and `PanicGroup.Counter()` create avoidable allocations that are paid often enough to matter. If those copies were part of per-request goroutine setup, replacing them with in-place mutation or prebuilt values could reduce allocation churn.

## Trigger

Start the daemon and ingestion service, then issue `getTransactions` requests against the already-running process.

## Target Code

- `cmd/stellar-rpc/internal/util/panicgroup.go:(*PanicGroup).Log:37-44` — allocates a fresh `PanicGroup` copy
- `cmd/stellar-rpc/internal/util/panicgroup.go:(*PanicGroup).Counter:46-53` — allocates a fresh `PanicGroup` copy
- `cmd/stellar-rpc/internal/daemon/daemon.go:(*Daemon).Run:537-550` — builds the panic group once when starting servers
- `cmd/stellar-rpc/internal/ingest/service.go:(*Service).Start:98-100` — builds the panic group once when starting ingestion

## Evidence

Both builder methods return a newly allocated `*PanicGroup` rather than mutating in place, so they do create extra objects.

## Anti-Evidence

The only production call sites are daemon startup and ingestion startup. There is no `PanicGroup` construction anywhere in the `getTransactions` request path, so the allocations are paid once per process lifetime, not once per request.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The builder allocations happen only during process startup, so removing them would not produce measurable `getTransactions` latency or RPS gains.

### Lesson Learned

Startup-only util allocations are out of scope for this objective even if they are technically avoidable; optimization hypotheses need repeated request-path cost.
