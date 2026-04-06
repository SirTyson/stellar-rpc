# H003: Fast getTransactions requests leave warning and limit timers live until they expire

**Date**: 2026-04-06
**Subsystem**: network
**Severity**: High
**Impact**: memory / GC / scheduler timer pressure
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once a `getTransactions` request finishes or is rejected, its warning and limit timers should be stopped immediately so the runtime does not keep timer state alive for a request that is already over. Fast successful requests should leave behind no pending timer work.

## Mechanism

Both duration limiters keep only the timer channels returned by `time.NewTimer(...).C`; the underlying `*time.Timer` handles are discarded, so the code cannot call `Stop()` on success. That means the warning and limit timers remain scheduled until they naturally fire even if `getTransactions` completed in milliseconds. Because the endpoint traverses both the outer HTTP limiter and the inner JSON-RPC limiter, each fast request can leave multiple timers outstanding for seconds after completion, inflating runtime timer-heap pressure and GC load at high QPS.

## Trigger

Run a sustained stream of fast `getTransactions` requests that complete well before the configured warning and limit thresholds. Track outstanding timers or heap growth, then compare against a version that retains the `*time.Timer` values and stops/drains them on every non-timeout exit path.

## Target Code

- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:(*httpRequestDurationLimiter).ServeHTTP:130-136` — creates warning and limit timers but stores only their channels
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:(*httpRequestDurationLimiter).ServeHTTP:175-194` — returns on completion without stopping those timers
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:(*RPCRequestDurationLimiter).Handle:238-244` — same timer creation pattern on the JSON-RPC layer
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:(*RPCRequestDurationLimiter).Handle:286-300` — returns on completion without stopping or draining

## Evidence

The code assigns `warningCh = time.NewTimer(...).C` and `limitCh = time.NewTimer(...).C` and never keeps a reference to the timer objects. The default config leaves these timers alive for meaningful periods (`max-request-execution-duration` defaults to 25s and `max-get-transactions-execution-duration` defaults to 5s), so a high-rate stream of quick `getTransactions` calls can accumulate a large timer backlog even when the endpoint is healthy.

## Anti-Evidence

If operators disable the limiters with the `NoLimit` sentinels, the issue disappears entirely. On very slow requests that naturally reach the warning or limit thresholds, the wasted timer lifetime is smaller because the timers would have fired anyway.

---

## Review

**Verdict**: VIABLE
**Severity**: Informational
**Date**: 2026-04-06
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced both duration limiter paths in `requestdurationlimiter.go`. Confirmed that `time.NewTimer(q.warningThreshold).C` (HTTP line 132, JRPC line 240) and `time.NewTimer(q.limitThreshold).C` (HTTP line 136, JRPC line 244) discard the `*time.Timer` handle, storing only the receive-only channel. On successful request completion (HTTP lines 175-194, JRPC lines 286-300), the function returns without any way to call `Stop()` on these timers. Each `getTransactions` request passes through both the HTTP layer (default warning=5s, limit=25s) and the JRPC layer (default warning≈1.67s, limit=5s), creating up to 4 unstopped timers per request. The `context.WithTimeout` timer at lines 139/251 is properly cleaned up by `defer requestCtxCancel()`, but the explicit timers are not.

### Code Paths Examined

- `requestdurationlimiter.go:ServeHTTP:130-136` — HTTP layer creates `warningCh` via `time.NewTimer(q.warningThreshold).C` and `limitCh` via `time.NewTimer(q.limitThreshold).C`. The `*time.Timer` return value is used only for `.C` access and immediately dropped.
- `requestdurationlimiter.go:ServeHTTP:175-194` — On successful completion (`requestCompleted` channel receives), the function writes the buffered response and returns. No timer cleanup occurs.
- `requestdurationlimiter.go:Handle:238-244` — JRPC layer has identical pattern: both timer handles are discarded after `.C` access.
- `requestdurationlimiter.go:Handle:286-300` — On successful completion, returns result directly. No timer cleanup.
- `requestdurationlimiter.go:ServeHTTP:139-140` — `context.WithTimeout` creates its own internal timer, but `defer requestCtxCancel()` properly stops it. This is the correct pattern that the explicit timers fail to follow.
- `jsonrpc.go:309` — JRPC warning threshold is `requestDurationLimit / 3`, so for getTransactions (5s limit), warning fires at ~1.67s.
- `config/options.go:525,531,579` — Confirmed defaults: HTTP warning=5s, HTTP limit=25s, getTransactions limit=5s.

### Findings

**The inefficiency is real and confirmed.** The `*time.Timer` handles are indeed discarded, making `Stop()` impossible. On every successful getTransactions request, up to 4 timers remain in the Go runtime timer heap until they naturally expire:

1. **HTTP warning timer**: lives ~5s after request completes (default `RequestExecutionWarningThreshold`)
2. **HTTP limit timer**: lives ~25s after request completes (default `MaxRequestExecutionDuration`)
3. **JRPC warning timer**: lives ~1.67s after request completes (`MaxGetTransactionsExecutionDuration / 3`)
4. **JRPC limit timer**: lives ~5s after request completes (default `MaxGetTransactionsExecutionDuration`)

At 1000 RPS, this means ~4000 unstopped timers accumulating in the runtime timer heap (weighted by their respective lifetimes: ~1000*(5+25+1.67+5) ≈ 36,670 timer-seconds of outstanding heap presence at steady state). Each timer still fires at its scheduled time, performing a no-op channel send (in Go 1.23+ the unbuffered channel drops the value; in older versions it sits in the buffer until GC).

**However, severity is Informational, not High.** The actual per-request cost of these lingering timers is negligible compared to getTransactions work:
- Stopping a timer costs ~50-100ns. Saving 4 `Stop()` calls means ~200-400ns saved per request.
- The timer heap operations (insertion already happened; the extra cost is just the delayed removal/firing) add ~100-200ns per timer fire.
- At 1000 RPS, total CPU savings: ~0.4-1.6ms/s — well under 0.01% of total CPU.
- Memory: ~100 bytes per timer × 36,670 outstanding ≈ 3.6 MB. Meaningful but not dominant.
- GC pressure: the extra long-lived timer objects do add GC scanning work, but this is dominated by the much larger response buffers and transaction data.

The fix is trivially correct and follows Go best practices, but the measurable impact on getTransactions latency or RPS would be negligible. This is a code hygiene improvement, not a performance win.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/network/requestdurationlimiter.go` — both `ServeHTTP` (lines 130-136) and `Handle` (lines 238-244).
- **Change description**: Retain the `*time.Timer` values instead of discarding them after `.C` access. Call `timer.Stop()` on all non-timeout exit paths (successful completion and panic recovery). Example for HTTP layer:
  ```go
  var warningTimer *time.Timer
  if q.warningThreshold != time.Duration(0) && q.warningThreshold < q.limitThreshold {
      warningTimer = time.NewTimer(q.warningThreshold)
      warningCh = warningTimer.C
  }
  var limitTimer *time.Timer
  if q.limitThreshold != time.Duration(0) {
      limitTimer = time.NewTimer(q.limitThreshold)
      limitCh = limitTimer.C
  }
  // ... in the requestCompleted case:
  if warningTimer != nil { warningTimer.Stop() }
  if limitTimer != nil { limitTimer.Stop() }
  ```
  Apply the same pattern to the JRPC `Handle` method.
- **Correctness check**: Existing tests in `requestdurationlimiter_test.go` cover normal completion, timeout, warning, and panic paths. All should pass unchanged since `Stop()` on an already-fired timer is a no-op.
- **Benchmark focus**: Measure runtime timer count (`runtime/metrics` or `debug.ReadGCStats`) under sustained load. The timer backlog should drop to near-zero. Latency/RPS impact will be negligible — expect < 0.01% change. The primary value is reduced GC scanning and timer heap size, not latency.
