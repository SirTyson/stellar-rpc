# H004: Request ID Allocation Survives the Debug-Logging Guard

**Date**: 2026-04-07
**Subsystem**: daemon
**Severity**: Low
**Impact**: per-request allocation waste
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When the daemon is running at its normal `info` level, `getTransactions` should avoid generating debug-only metadata that will never be logged. Request identifiers used exclusively by `logRequest` and `logResponse` should be created lazily only when debug logging is enabled.

## Mechanism

`decorateHandlers` now guards both `logRequest` and `logResponse` behind `debugEnabled`, but it still calls `middleware.NextRequestID()` and `strconv.FormatUint()` before that check. That leaves a small but guaranteed atomic increment and string allocation on every `getTransactions` request in normal production logging mode, even though the value is unused.

## Trigger

Run an `info`-level `getTransactions` benchmark with allocation profiling and compare it to a build that moves request-ID creation inside the `if debugEnabled` blocks.

## Target Code

- `cmd/stellar-rpc/internal/jsonrpc.go:decorateHandlers:77-120` — eagerly computes `reqID` at line 90 even though lines 91-93 and 112-114 are the only consumers.

## Evidence

The code unconditionally executes `strconv.FormatUint(middleware.NextRequestID(), 10)` before checking `debugEnabled`. A recent optimization already removed the much larger cost of marshaling successful responses for suppressed debug logs, which makes this leftover debug-only allocation the next obvious dead-work candidate in the same wrapper.

## Anti-Evidence

This is a micro-optimization compared with the DB scan and XDR/JSON work inside `getTransactions`, so the improvement may stay below five percent. Its value is strongest when paired with other wrapper cleanups that chip away at fixed per-request overhead.
