# H002: Duration Limiters Leave Completed getTransactions Timers Live Until They Fire

**Date**: 2026-04-07
**Subsystem**: daemon
**Severity**: Medium
**Impact**: timer heap / GC pressure
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once a `getTransactions` request finishes, any warning or deadline timers created to police that request should be stopped immediately so they disappear from the runtime timer heap. Completed requests should not continue consuming timer-queue space or wakeup work after their response has already been returned.

## Mechanism

Both `(*RPCRequestDurationLimiter).Handle` and `(*httpRequestDurationLimiter).ServeHTTP` create timers with `time.NewTimer(...).C` and discard the `*Timer`, which means there is no way to call `Stop()` on the fast-success path. Because `getTransactions` is wrapped by both limiters, each normal request schedules four timers (JRPC warn/limit plus HTTP warn/limit) that remain live until they fire, creating avoidable runtime timer churn and heap retention under sustained load.

## Trigger

Run a sustained `getTransactions` benchmark where most requests finish well under five seconds and inspect heap/timer profiles, or compare CPU and allocation behavior after changing both limiters to keep timer handles and stop/drain them on the `requestCompleted` path.

## Target Code

- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:(*httpRequestDurationLimiter).ServeHTTP:130-137,175-194` — creates warning/limit timers and returns on completion without stopping them.
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:(*RPCRequestDurationLimiter).Handle:238-245,286-300` — repeats the same pattern for the per-method limiter.
- `cmd/stellar-rpc/internal/jsonrpc.go:NewJSONRPCHandler:326-374` — composes the per-method and global duration limiters together on the `getTransactions` path.

## Evidence

The code stores only the timer channels, not the timer objects, so neither limiter can reclaim timers when the request completes early. `NewJSONRPCHandler` sets the per-method warning threshold to one third of the method limit and also installs the global HTTP warning/limit pair, so a healthy `getTransactions` request creates multiple timers that outlive the request itself.

## Anti-Evidence

Go timers are fairly cheap, and the benefit may only emerge at higher request rates where thousands of recently completed requests overlap in the timer heap. If `getTransactions` spends most of its time in DB/XDR work, timer churn may show up more in tail latency and CPU profiles than in median latency.
