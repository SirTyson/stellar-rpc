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
