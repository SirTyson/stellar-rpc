# H008: Debug-only request param stringification is too small to matter for `getTransactions`

**Date**: 2026-04-07
**Subsystem**: network
**Severity**: Low
**Impact**: request-ingress allocation
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Any viable `getTransactions` logging optimization needs to remove enough hot-path work to measurably change end-to-end latency or throughput. Debug-only request-field preparation should only be worth pursuing if request-side bytes are large enough for that stringification and entry allocation to compete with response-side work.

## Mechanism

I investigated whether `logRequest()`'s debug path does enough eager work to matter because it calls `req.ParamString()` and `logger.WithField("params", ...)` before `logger.Debug(...)` decides whether the message will be emitted. The actual behavior does copy the request params bytes into a string and allocates another log entry on every request, but `getTransactions` request bodies are tiny compared with the response path and the already-known info-level logging costs.

## Trigger

Profile `getTransactions` at default `info` log level and isolate CPU/allocations from `req.ParamString()` plus the extra `WithField` entry construction inside `logRequest()`.

## Target Code

- `cmd/stellar-rpc/internal/jsonrpc.go:logRequest:110-121` — eagerly builds the debug-only `params` field before calling `Debug`
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/base.go:(*Request).ParamString:104-106` — converts `req.params` to a string

## Evidence

The code does real hot-path work even when debug logging is off: `ParamString()` copies the JSON params bytes into a string, and `WithField` constructs another `log.Entry` before `Debug()` checks the level. That made it worth checking while tracing the request logging path.

## Anti-Evidence

`getTransactions` params usually contain only `startLedger`, a small pagination object, and `format`, so the copied payload is tiny. The response path already pays much larger costs in info-level logging, bridge serialization, and response buffering, so this request-side debug preparation is unlikely to clear even the Low-severity bar.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The eager debug-field work is real, but the request payload for `getTransactions` is far too small for `ParamString()` and one extra `WithField` allocation to produce a measurable endpoint win.

### Lesson Learned

For `getTransactions`, logging findings need to target full response serialization or default info-level writes, not tiny request-side debug bookkeeping.
