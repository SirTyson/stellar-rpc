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
