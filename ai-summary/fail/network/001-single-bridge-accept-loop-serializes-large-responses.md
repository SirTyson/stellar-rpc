# H001: Single bridge accept loop serializes concurrent `getTransactions` response parsing

**Date**: 2026-04-07
**Subsystem**: network
**Severity**: High
**Impact**: response-path concurrency bottleneck / throughput loss
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Concurrent `getTransactions` requests that have already finished ledger reads and JSON construction should be able to complete their in-process response handoff independently. One large response should not force unrelated completed responses to wait behind a single parser goroutine before they can leave the bridge.

## Mechanism

`NewJSONRPCHandler` creates exactly one shared `jhttp.Bridge` for all HTTP traffic, and `server.NewLocal` gives that bridge one `jrpc2.Client`. `jrpc2.NewClient` starts a single accept loop that does `Recv()` and then `parseJSON(bits)` synchronously before it can receive the next response; because the bridge uses unbuffered `channel.Direct()` channels, every completed server response is backpressured behind that one parser. For large `getTransactions` JSON responses, this turns response decoding into a global head-of-line bottleneck that can cap RPS well before the actual handler, DB, or FFI work is saturated.

## Trigger

Run `getTransactions` with `xdrFormat=json`, `pagination.limit` near 200, and client concurrency well above 1 (for example 16-64 concurrent calls). Compare baseline against a version that shards requests across multiple local bridges or bypasses the shared bridge for `getTransactions`, and watch for lower response-serialization CPU time and higher zero-error RPS.

## Target Code

- `cmd/stellar-rpc/internal/jsonrpc.go:337-389` — constructs one shared `jhttp.Bridge` and installs it for all HTTP requests
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/server/local.go:26-34` — `NewLocal` creates a single client/server pair for that bridge
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/client.go:64-69` — `NewClient` launches one long-lived accept goroutine
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/client.go:76-101` — `accept` does `Recv()` and `parseJSON(bits)` serially before receiving the next response
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/channel/channel.go:84-90` — `direct.Send` blocks on an unbuffered channel send
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/channel/channel.go:111-116` — `Direct()` creates the unbuffered in-memory transport used by the bridge

## Evidence

The bridge is process-wide, not per-request. The only client receive loop is the goroutine started in `NewClient`, and it cannot issue the next `Recv()` until it finishes `in.parseJSON(bits)` for the current response. Since `channel.Direct` is synchronous, completed server-side responses cannot even hand off their bytes until that goroutine is ready again, so one multi-megabyte `getTransactions` reply can delay every other completed reply behind it.

## Anti-Evidence

If operators primarily use small `limit` values or the XDR response format, the serialized parse step is smaller and the bottleneck may not dominate. A more invasive direct-dispatch bridge would also remove this bottleneck entirely, so the cleanest long-term fix may subsume a sharded-bridge approach.

---

## Review

**Verdict**: VIABLE
**Severity**: Medium
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated. Related to reviewed/002 (per-request bridge round-trip overhead) but addresses a distinct impact: cumulative head-of-line blocking across concurrent responses, not per-request serialization cost. Also related to reviewed/001 (hidden concurrency cap) which limits handler parallelism — the current finding shows that even with the concurrency cap raised, response delivery remains serialized through one accept loop.

### Trace Summary

Traced the full response delivery path from the jrpc2 server's `deliver()` through the unbuffered `channel.Direct()` channel to the client's single `accept()` goroutine. Confirmed a double serialization bottleneck: (1) the server holds `s.mu` during `encode()` → `ch.Send(bits)`, which blocks on the unbuffered channel until the accept goroutine calls `Recv()`; (2) the accept goroutine serially calls `Recv()` + `parseJSON(bits)` for each response before it can receive the next one. With concurrent handlers completing around the same time, responses queue behind both the server mutex and the accept goroutine's parse step, creating a hard throughput ceiling proportional to the per-response parse time.

### Code Paths Examined

- `cmd/stellar-rpc/internal/jsonrpc.go:337-342` — confirmed: single `jhttp.NewBridge(...)` creates one bridge for all HTTP traffic, no sharding or per-method bridges
- `jhttp/bridge.go:NewBridge:189-206` — confirmed: calls `server.NewLocal()` which creates one `jrpc2.Server` + one `jrpc2.Client` on one `channel.Direct()` pair
- `server/local.go:NewLocal:26-34` — confirmed: single `channel.Direct()` creates unbuffered `chan []byte` pair
- `channel/channel.go:Direct:111-117` — confirmed: `make(chan []byte)` (unbuffered), so `Send` blocks until `Recv` is called
- `channel/channel.go:direct.Send:84-91` — confirmed: `d.send <- msg` blocks on unbuffered channel
- `client.go:NewClient:64-69` — confirmed: spawns exactly one goroutine running `c.accept(ch)` in a loop
- `client.go:accept:76-101` — confirmed: serial `Recv()` → `parseJSON(bits)` → spawn delivery goroutine → loop back to `Recv()`. No parallelism in receive+parse.
- `json.go:jmessages.parseJSON:87-115` — confirmed: `json.Unmarshal(data, &msgs[0])` copies bytes; then `jmessage.parseJSON(raw)` does `json.Unmarshal(data, &map[string]json.RawMessage{})` which scans the entire envelope
- `json.go:jmessage.parseJSON:198-264` — confirmed: `json.Unmarshal(data, &obj)` scans every byte to find key-value boundaries. For a ~2MB result field, the scanner must walk all bytes to find the closing brace.
- `server.go:deliver:284-304` — **critical**: holds `s.mu` via `s.mu.Lock()` / `defer s.mu.Unlock()` while calling `encode(ch, rsps)` → `ch.Send(bits)`. The `Send` blocks until the accept goroutine calls `Recv()`, so `s.mu` is held during the entire channel rendezvous + however long the accept goroutine takes to finish parsing the previous response.
- `server.go:serve:170-183` — confirmed: each request batch dispatch is a separate goroutine, so multiple `deliver()` calls can contend on `s.mu` simultaneously

### Findings

**The head-of-line blocking mechanism is confirmed and architecturally distinct from H002's per-request overhead.**

1. **Server-side mutex during blocking send**: `Server.deliver()` holds `s.mu` while calling `ch.Send(bits)` on the unbuffered channel. This means only one response can be "in flight" on the server side at a time. While one response's `Send` is blocked waiting for the accept goroutine, all other completed handlers queue on `s.mu`.

2. **Client-side serial accept loop**: The single `accept()` goroutine processes responses strictly sequentially: `Recv()` → `parseJSON()` → spawn delivery → `Recv()`. The `parseJSON` step for a large getTransactions response (~2 MB JSON) involves `json.Unmarshal` into `map[string]json.RawMessage`, which must scan every byte of the response to find field boundaries. At Go's JSON scanner throughput (~200-500 MB/s), this takes ~4-10 ms per 2 MB response.

3. **Throughput ceiling calculation**: Each response occupies the serialized path for approximately: `toJSON` (~1 ms) + channel rendezvous (~0 ms) + `parseJSON` (~5-10 ms) = ~6-11 ms. The accept loop can therefore process ~91-167 responses/sec. With 16 concurrent connections at steady state, each connection gets ~6-10 responses/sec, imposing a ~100-167 ms per-response bridge delay.

4. **Cumulative queueing at high concurrency**: If 16 handlers complete at similar times (likely when handler execution time is similar), the Nth response waits approximately (N-1) × 10 ms in the serialized queue. The 16th response incurs ~150 ms of bridge queueing delay. If handler execution is ~50 ms, this represents a ~3x increase in tail latency.

5. **Distinction from H002**: H002 identifies ~10-25 ms of per-request bridge overhead (7-17% of latency). The current finding shows that at 16+ concurrency, the **cumulative** overhead from serialized accept-loop processing can reach ~150 ms for tail requests — far exceeding the per-request cost. H002's direct-dispatch fix would eliminate both findings; the current hypothesis's sharding fix addresses only the concurrency bottleneck while keeping the per-request bridge round-trip.

**Severity assessment**: Medium. The throughput ceiling is real and impactful at high concurrency with large JSON responses. Under the specified conditions (JSON format, limit=200, concurrency 16+), the improvement from sharding would be >20%. However: (a) these are specific conditions that may not represent all workloads; (b) H002's direct-dispatch fix, if implemented, would subsume this finding entirely; (c) the independent value of bridge sharding is as a lower-risk alternative to full bridge bypass.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/jsonrpc.go:337-342` — replace the single `jhttp.NewBridge(...)` call with a pool of N bridges (e.g., N=4 or `runtime.NumCPU()`), round-robining HTTP requests across them.
- **Change description**: Create `[]jhttp.Bridge` instead of a single `jhttp.Bridge`. Each bridge gets the same `decorateHandlers(...)` assigner and `bridgeOptions`. Add a request counter (atomic uint64) and select bridge via `counter % N`. The `Handler` struct's `Close()` must close all bridges. The backlog limiter and duration limiter wrappers remain shared across all bridges (they use atomics, not per-bridge state).
- **Correctness check**: All existing tests in `cmd/stellar-rpc/internal/integrationtest/` and unit tests must pass. The bridge sharding must be transparent to callers — each bridge independently handles the full JSON-RPC protocol. Key correctness concern: the jrpc2 bridge virtualizes request IDs internally (see `serveInternal` lines 88-124), so cross-bridge ID conflicts cannot occur since each bridge has its own ID namespace.
- **Benchmark focus**: Measure `getTransactions` RPS and p50/p99 latency at concurrency 16, 32, 64 with `format=json&limit=200`. Compare N=1 (baseline) vs N=4 and N=8 bridges. Expect the throughput ceiling to scale approximately linearly with N. Also compare against H002's direct-dispatch approach — if direct dispatch is feasible, it eliminates the accept loop entirely and should outperform sharding.

---

## PoC Attempt

**Result**: POC_FAIL
**Date**: 2026-04-07
**PoC by**: claude-opus-4-6, high
**Failed At**: poc
**Iterations**: 0

### Failure Reason

The optimization target no longer exists. The codebase has already replaced `jhttp.NewBridge(...)` with a custom `directBridge` implementation (`cmd/stellar-rpc/internal/directbridge.go`) that completely bypasses the jrpc2 client/server architecture. The `directBridge`:

- Dispatches JSON-RPC requests directly to method handlers without going through `server.NewLocal()`, `channel.Direct()`, or `jrpc2.Client`
- Has no accept loop, no unbuffered channels, and no server-side mutex blocking response delivery
- Eliminates the entire serialized response path that the hypothesis identified as the bottleneck

This is exactly the "more invasive direct-dispatch bridge" mentioned in the hypothesis's Anti-Evidence section. There is no `jhttp.Bridge` to shard — the bottleneck has been architecturally eliminated. The suggested PoC approach (sharding N bridges with round-robin) cannot be implemented because the underlying infrastructure (`jhttp.Bridge`, `server.NewLocal`, `channel.Direct`) is no longer used anywhere in the production code.

Verified: `grep -r 'jhttp\.NewBridge\|jhttp\.Bridge' cmd/stellar-rpc/internal/` returns no matches. The only reference to these components is a comment in `directbridge.go` explaining what it replaces.

### Changes Attempted

No source code changes were made. The optimization target (single `jhttp.Bridge`) does not exist in the current codebase, so there was nothing to modify. The hypothesis's finding was valid at the time of analysis but has been superseded by the `directBridge` implementation.
