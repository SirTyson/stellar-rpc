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

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated
**Failed At**: reviewer

### Trace Summary

Traced both `ServeHTTP` (lines 124-197) and `Handle` (lines 233-303) in `requestdurationlimiter.go`. Confirmed both use the `time.NewTimer(d).C` pattern that discards the `*Timer` handle and stores only the channel. However, the project uses Go 1.25 (`go.mod` line 3), which includes the Go 1.23 timer garbage collection improvement. In Go 1.23+, unreferenced `Timer` and `Ticker` objects are eligible for immediate garbage collection even without calling `Stop()`. Since the `*Timer` is never assigned to a variable (only `.C` is retained), the Timer becomes unreferenced immediately after the `.C` access and is collected at the next GC cycle — NOT when it fires.

### Code Paths Examined

- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:ServeHTTP:130-137` — confirmed: `time.NewTimer(q.warningThreshold).C` and `time.NewTimer(q.limitThreshold).C` discard the `*Timer`, storing only the channel
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:ServeHTTP:175-194` — on completion via `requestCompleted`, returns without stopping timers; `warningCh` and `limitCh` go out of scope
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:Handle:238-245` — same `time.NewTimer(d).C` pattern as the HTTP limiter
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:Handle:286-300` — on completion, returns without stopping timers
- `cmd/stellar-rpc/internal/jsonrpc.go:NewJSONRPCHandler:328-335` — per-method JRPC limiter wraps each handler
- `cmd/stellar-rpc/internal/jsonrpc.go:NewJSONRPCHandler:368-374` — global HTTP limiter wraps the entire bridge
- `go.mod:3` — `go 1.25`, confirming Go 1.23+ timer GC behavior applies

### Why It Failed

The claimed mechanism — timers lingering in the runtime timer heap for their full duration (5–25 seconds) after requests complete — does not occur in Go 1.23+. The Go 1.23 release introduced automatic garbage collection of unreferenced timers: "Timers and Tickers created with NewTimer, AfterFunc, NewTicker, and Tick can now be garbage collected sooner, even if their Stop methods have not been called." In the `time.NewTimer(d).C` pattern, the `*Timer` is immediately unreferenced (only `.C` is assigned to a local variable), making the timer eligible for GC at the next cycle — typically within milliseconds, not the 5–25 seconds claimed. With Go 1.25, there is no timer heap bloat, no sustained timer churn, and negligible GC pressure from this pattern.

### Lesson Learned

When analyzing timer lifecycle issues in Go codebases, always check the Go version. The `time.NewTimer(d).C` pattern was a genuine problem in Go <1.23 (timers stayed in the heap until they fired) but is a non-issue in Go ≥1.23 where unreferenced timers are collected automatically. The project's Go 1.25 makes this hypothesis's core mechanism invalid.
