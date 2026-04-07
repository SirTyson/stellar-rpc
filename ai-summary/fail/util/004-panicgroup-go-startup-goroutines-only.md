# H004: PanicGroup Go Wrapper Never Encloses getTransactions Work

**Date**: 2026-04-07
**Subsystem**: util
**Severity**: Informational
**Impact**: request scheduling / allocation overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Any util optimization that improves `getTransactions` latency should remove work performed while the endpoint is actively serving requests. If `PanicGroup.Go()` wrapped request-scoped helpers, eliminating its extra goroutine, closure capture, and deferred recovery frame could reduce scheduler and allocation overhead on every call.

## Mechanism

I investigated whether `PanicGroup.Go()` participates in the steady-state `getTransactions` call chain. The wrapper always creates a fresh goroutine and a deferred `recoverRoutine`, so repeated use around request work would add measurable runtime overhead.

## Trigger

Run sustained successful `getTransactions` traffic against an already-started daemon and inspect whether request execution passes through `PanicGroup.Go()`.

## Target Code

- `cmd/stellar-rpc/internal/util/panicgroup.go:(*PanicGroup).Go:55-61` — launches a new goroutine with deferred panic recovery
- `cmd/stellar-rpc/internal/daemon/daemon.go:(*Daemon).Run:537-553` — uses `PanicGroup.Go()` only to start the HTTP servers
- `cmd/stellar-rpc/internal/ingest/service.go:(*Service).Start:94-121` — uses `PanicGroup.Go()` only to start the ingestion loop
- `cmd/stellar-rpc/internal/methods/get_transactions.go:(transactionsRPCHandler).getTransactionsByLedgerSequence:251-387` — the actual request path being optimized

## Evidence

`PanicGroup.Go()` definitely adds runtime work: it allocates a goroutine stack, captures `fn`, and installs a deferred recovery handler. Those costs would matter if they occurred per request or per ledger batch.

## Anti-Evidence

The only production callers are `Daemon.Run()` and `Service.Start()`, both of which execute once during startup. The `getTransactions` request handler runs synchronously through the JSON-RPC handler stack and never invokes `PanicGroup.Go()`.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

`PanicGroup.Go()` is not on the `getTransactions` steady-state execution path, so removing its goroutine-wrapper overhead would not change endpoint latency or throughput.

### Lesson Learned

For util performance work, separate "goroutines that keep the service alive" from "goroutines created while serving a request" before treating launcher overhead as a hot-path optimization target.
