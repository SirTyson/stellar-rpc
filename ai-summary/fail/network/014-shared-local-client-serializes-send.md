# H002: The shared in-process bridge client serializes concurrent `getTransactions` requests on one mutex

**Date**: 2026-04-07
**Subsystem**: network
**Severity**: Medium
**Impact**: bridge lock contention / head-of-line blocking
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Independent HTTP `getTransactions` requests should be able to enter the in-process JSON-RPC bridge concurrently. The bridge should not force all callers through a single mutex-protected handoff path before they can even reach the method-specific backlog and duration limiters.

## Mechanism

`jhttp.Bridge` owns one long-lived `server.Local`, and every HTTP request calls `b.local.Client.Batch(...)` on that shared client. Inside jrpc2, `Client.send` holds `c.mu` while it sends the JSON-RPC batch to `channel.Direct`, and `channel.Direct` is an unbuffered in-memory pipe. Because the same shared client is used for all HTTP requests, concurrent `getTransactions` calls serialize on that mutex-plus-rendezvous path, adding head-of-line blocking before the real handler work begins.

## Trigger

Drive many concurrent single-request `getTransactions` POSTs and capture mutex/block profiles around the bridge. Compare baseline behavior with a direct-dispatch bridge or another design that does not funnel every request through the shared `local.Client.send` path. The issue is confirmed if lock contention in `Client.send`/`deliverLocked` rises with concurrency and admitted-request latency drops when that shared client is removed from the path.

## Target Code

- `cmd/stellar-rpc/internal/jsonrpc.go:325-329` — stellar-rpc constructs one shared `jhttp.Bridge` for all HTTP requests
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/jhttp/bridge.go:37-40` — `Bridge` stores a single shared `local server.Local`
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/jhttp/bridge.go:126-138` — each HTTP request enters `b.local.Client.Batch(...)`
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/client.go:30-35` — the client has one shared mutex and one shared pending-response map
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/client.go:204-244` — `Client.send` locks `c.mu`, sends the batch, and registers pendings while still under that client-wide lock
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/client.go:335-357` — `Batch` uses `send` even for the common single-request POST path
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/channel/channel.go:104-116` — `channel.Direct` is implemented with unbuffered Go channels, so senders rendezvous synchronously with the receiver

## Evidence

The bridge is not using per-request client state; it reuses one `local.Client` across all HTTP callers. jrpc2's own channel contract explicitly says a channel is only required to support one sender and one receiver concurrently, which explains why the client protects send with a mutex and why every HTTP request is forced through the same serialized handoff point.

## Anti-Evidence

The send critical section is still much shorter than `getTransactions` execution itself, so the win may show up mainly under high concurrency rather than in single-request latency. A direct bridge implementation that fixes this will overlap with the already known bridge round-trip issue, so the eventual code change may collapse multiple hypotheses into one larger refactor.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated (related to H010 which covered request-side marshaling CPU cost, but H002 is specifically about mutex contention under concurrency — a distinct claim)
**Failed At**: reviewer

### Trace Summary

Traced the complete request path from `Bridge.ServeHTTP` through `Client.Batch` → `Client.send` → `channel.Direct.Send` and the server's `read` goroutine loop. Also traced the response delivery path through `Server.deliver` → `encode` → `channel.Direct.Send` and the client's `accept` → `deliverLocked` path. The critical section under `c.mu` in `Client.send` is real but extremely short — the heavy request marshaling (`reqs.toJSON()`) happens OUTSIDE the lock (line 212), and the server's `read` goroutine loops tightly (parse + enqueue, ~1-2µs per iteration), meaning the unbuffered channel rendezvous in `ch.Send(b)` completes almost instantly.

### Code Paths Examined

- `jhttp/bridge.go:76-149 (serveInternal)` — confirmed each HTTP request calls `b.local.Client.Batch()` on the shared client
- `client.go:204-244 (Client.send)` — confirmed the critical section: lock at line 227, `ch.Send(b)` at line 233, pending registration at lines 240-243, unlock via defer. Crucially, the expensive `reqs.toJSON()` marshaling happens at line 212, BEFORE the lock
- `client.go:335-357 (Batch)` — confirmed request construction (`c.req`) acquires `c.mu` briefly for ID increment, then `c.send` acquires it again for the channel send
- `channel/channel.go:88-116 (direct)` — confirmed unbuffered Go channels (`make(chan []byte)`) with synchronous rendezvous semantics
- `server.go:634-669 (Server.read)` — confirmed the reader loops tightly: `ch.Recv()` → `parseJSON` → lock → enqueue → unlock → back to `Recv()`. Processing between consecutive `Recv()` calls is ~1-2µs
- `server.go:170-183 (Server.serve)` — confirmed handler dispatch is fully asynchronous: `go func() { next() }()` — handlers run in their own goroutines, NOT under any lock
- `client.go:76-103 (Client.accept)` — confirmed response delivery spawns a goroutine that acquires `c.mu` for `deliverLocked`, but this is also trivially fast (map lookup + channel send)

### Why It Failed

The hypothesis correctly identifies the serialization mechanism but fundamentally mischaracterizes its impact:

1. **No head-of-line blocking from slow handlers.** The hypothesis claims requests experience "head-of-line blocking before the real handler work begins," implying a slow `getTransactions` handler blocks subsequent requests at the bridge. This is false. The `Client.send` critical section covers only the channel send and pending registration — handler execution is fully asynchronous in its own goroutine (server.go:178-181). A slow handler does not hold `c.mu`.

2. **The critical section is microseconds, not milliseconds.** Under `c.mu`, the only blocking operation is `ch.Send(b)` on the unbuffered channel. The server's `read` goroutine is nearly always parked in `ch.Recv()`, so the rendezvous completes in ~1µs. The total critical section (error check + send + map insert) is ~2-5µs. Even at 100 concurrent requests, worst-case queuing delay is ~200-500µs — under 1% of a typical `getTransactions` response time of 10-50ms.

3. **Request marshaling is not serialized.** The expensive `reqs.toJSON()` call at line 212 executes BEFORE acquiring `c.mu`. For `getTransactions` requests (small payloads), this is fast anyway, but the design ensures the lock is never held during marshaling.

4. **Impact is far below Medium severity.** Even in the most generous estimate (1000 concurrent requests, 5µs critical section), average added latency is ~2.5ms. Against a 50ms `getTransactions` response, that's 5% — borderline, and only at extreme concurrency that would already be throttled by the backlog queue limiter. At realistic concurrency levels (10-100), the impact is well under 1%.

### Lesson Learned

When analyzing mutex contention, examine what work is actually done under the lock versus what appears to be "in the path." In jrpc2's `Client.send`, the expensive operations (JSON marshaling, handler execution) are intentionally kept outside the critical section, leaving only a fast channel rendezvous and bookkeeping under the lock. The presence of a shared mutex does not imply meaningful contention unless the critical section duration is significant relative to the overall operation cost.
