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

---

## Review

**Verdict**: VIABLE
**Severity**: Informational
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

The `decorateHandlers` function (jsonrpc.go:77-120) wraps every JSON-RPC handler with metrics and optional debug logging. On line 90, it unconditionally calls `strconv.FormatUint(middleware.NextRequestID(), 10)`, which performs an `atomic.AddUint64` on a global counter (chi middleware/request_id.go:95) and formats the result as a decimal string. The resulting `reqID` is only consumed inside two `if debugEnabled` guards (lines 91-93 and 112-114). In production, `debugEnabled` is false (set from `cfg.LogLevel >= logrus.DebugLevel` at line 340), so the atomic increment and string allocation are pure waste on every request.

### Code Paths Examined

- `cmd/stellar-rpc/internal/jsonrpc.go:decorateHandlers:77-120` — confirmed `reqID` is computed at line 90, only consumed at lines 92 and 113, both inside `if debugEnabled` blocks
- `cmd/stellar-rpc/internal/jsonrpc.go:337-341` — `debugEnabled` is set to `cfg.LogLevel >= logrus.DebugLevel` at handler creation time, captured as a closure constant
- `go-chi/chi/middleware/request_id.go:NextRequestID:94-96` — `atomic.AddUint64(&reqid, 1)`, a single lock-prefixed x86 instruction (~5-20ns depending on contention)
- `cmd/stellar-rpc/internal/jsonrpc.go:logRequest:122-134` — only consumer of reqID pre-handler; uses it as a log field
- `cmd/stellar-rpc/internal/jsonrpc.go:logResponse:136-154` — only consumer of reqID post-handler; uses it as a log field

### Findings

The inefficiency is real and the fix is trivially correct: move line 90 inside both `if debugEnabled` blocks (or compute it once in the first block and reuse via a closure variable). The fix cannot break correctness because:

1. `reqID` has no consumers outside the debug-logging guards
2. The atomic counter increment has no semantic purpose beyond generating unique debug log correlation IDs
3. No API contract depends on monotonic request ID generation

However, the absolute cost is negligible:
- `atomic.AddUint64`: ~10ns per call (single cache-line-bouncing instruction)
- `strconv.FormatUint`: ~30-50ns per call (small heap allocation of ≤20 bytes)
- Total waste: ~50-70ns per request

For a `getTransactions` request that involves SQLite reads, XDR deserialization, and JSON serialization (typically 1-100ms), this represents <0.01% of total latency. At 10,000 RPS, total CPU savings would be ~0.5ms per second — well below noise level. The improvement will not appear in any benchmark.

**Severity downgraded from Low to Informational**: the hypothesis claims "Low" (<5% but measurable), but the actual impact is sub-nanosecond-fraction of request time, making it theoretical rather than measurable.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/jsonrpc.go:89-114` — the closure inside `decorateHandlers`
- **Change description**: Move `reqID := strconv.FormatUint(middleware.NextRequestID(), 10)` inside the first `if debugEnabled` block, and capture it in a variable accessible to the second block. Alternatively, compute it in both blocks independently (the counter value doesn't need to match since debug logging is the only consumer).
- **Correctness check**: Any existing test that exercises the JSON-RPC handler path should pass unchanged. Run `make go-test` to verify no regressions.
- **Benchmark focus**: One allocation per request should disappear from `go test -bench -benchmem` profiles. The latency improvement will be in the single-digit nanosecond range and likely invisible in end-to-end benchmarks. Focus on `allocs/op` reduction rather than time.
