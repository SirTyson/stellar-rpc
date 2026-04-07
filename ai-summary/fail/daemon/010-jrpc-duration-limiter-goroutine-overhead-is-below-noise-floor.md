# H010: JRPC Duration Limiter Goroutine Overhead Is a Meaningful getTransactions Bottleneck

**Date**: 2026-04-07
**Subsystem**: daemon
**Severity**: Low
**Impact**: per-request wrapper setup overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

The per-method duration limiter around `getTransactions` should add so little fixed setup cost that it does not materially affect throughput compared to the endpoint's DB, XDR, and JSON work. A wrapper-level goroutine or timeout context should only matter if it is large relative to the request body of work.

## Mechanism

I suspected `RPCRequestDurationLimiter.Handle` might be a worthwhile optimization target because it allocates a timeout context, a completion channel, and a goroutine for every `getTransactions` request before calling the real handler. If that wrapper cost were a significant fraction of total request time, removing the goroutine handoff or specializing the limiter for synchronous handlers could improve endpoint latency.

## Trigger

Compare `getTransactions` throughput with the current per-method limiter against a build that invokes the handler directly without `MakeJrpcRequestDurationLimiter`, while keeping the rest of the request path unchanged.

## Target Code

- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:233-301` — `Handle` creates `context.WithTimeout`, `requestCompleted`, and a goroutine for each request.
- `cmd/stellar-rpc/internal/jsonrpc.go:321-333` — every `getTransactions` request passes through the per-method duration limiter before entering the handler.

## Evidence

The limiter does perform fixed per-request work before any DB query begins: it creates a derived context, allocates a channel, launches a goroutine, and synchronizes the result back through `requestCompleted`.

## Anti-Evidence

`getTransactions` requests already spend milliseconds to tens of milliseconds in DB access, XDR decoding, hashing, base64 encoding, or JSON conversion, so one goroutine/context/channel setup is unlikely to reach even a few percent of end-to-end time. The queue limiter is also outermost, meaning overload rejections avoid this path entirely.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The wrapper cost is fixed O(1) setup work, while the `getTransactions` hot path is dominated by size-dependent DB and serialization work. Even if the limiter were made allocation-free, it would not plausibly produce a meaningful endpoint-level improvement compared to the larger payload-processing inefficiencies already visible in this code path.

### Lesson Learned

For daemon-side `getTransactions` optimization, prioritize work that scales with page size or ledger density. Fixed wrapper overheads around the handler are poor targets unless profiles show the endpoint is already otherwise close to memory-bandwidth limits.
