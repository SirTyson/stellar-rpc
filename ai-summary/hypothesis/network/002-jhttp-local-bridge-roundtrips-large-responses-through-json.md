# H002: The in-process `jhttp` bridge round-trips large `getTransactions` responses through JSON bytes

**Date**: 2026-04-07
**Subsystem**: network
**Severity**: Medium
**Impact**: in-process serialization CPU / allocation
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Because stellar-rpc's HTTP JSON-RPC bridge and method handlers live in the same process, a `getTransactions` response should not need to be serialized into JSON, sent across an in-memory channel, parsed back into protocol messages, and then serialized again just to reach the HTTP writer. The hot path should keep the large response in-process and encode it only once at the boundary that actually writes bytes to the client.

## Mechanism

`NewJSONRPCHandler` builds `jhttp.NewBridge(...)`, and `jhttp.NewBridge` in turn constructs `server.NewLocal`, an in-memory JSON-RPC client/server pair connected by `channel.Direct()`. For a large `getTransactions` response, the method result is first marshaled in `Server.invoke`, then wrapped into JSON-RPC reply bytes and sent through the local channel, then parsed back into `jmessage` objects by the local client, and finally marshaled again by the bridge before the HTTP layer sees it. Those encode/decode passes are all in-process overhead unrelated to ledger access or actual socket I/O.

## Trigger

Run large `getTransactions` requests (`format=json`, `limit` near 200) and compare current behavior with a custom direct POST bridge that dispatches to the handler map without `server.NewLocal`/`Client.Batch`. Measure CPU profiles, bytes allocated per request, and latency in the response-serialization phase.

## Target Code

- `cmd/stellar-rpc/internal/jsonrpc.go:325-329` — constructs the HTTP bridge with `jhttp.NewBridge(decorateHandlers(...))`
- `github.com/creachadair/jrpc2@v1.3.5/server/local.go:NewLocal:26-34` — creates an in-memory client/server pair via `channel.Direct()`
- `github.com/creachadair/jrpc2@v1.3.5/server.go:Server.invoke:372-389` — marshals the handler result with `json.Marshal(v)` after the method returns
- `github.com/creachadair/jrpc2@v1.3.5/server.go:tasks.responses:773-813` — wraps the raw result into JSON-RPC response messages
- `github.com/creachadair/jrpc2@v1.3.5/client.go:Client.send:198-239` — serializes the request batch to JSON and sends it on the in-memory channel
- `github.com/creachadair/jrpc2@v1.3.5/client.go:Client.accept:74-98` — parses the returned response bytes back into protocol messages
- `github.com/creachadair/jrpc2@v1.3.5/jhttp/bridge.go:Bridge.serveInternal:126-149` — re-marshals bridge responses for HTTP output

## Evidence

The bridge is not a direct function-call adapter; it explicitly stands up a local JSON-RPC transport and moves every HTTP request through JSON frames. `getTransactions` is one of the worst-case consumers of that design because the response body is large and already JSON-heavy, so every extra encode/decode pass scans and copies multi-megabyte payloads.

## Anti-Evidence

The request body itself is small, so most of the win comes from large responses rather than from request ingress. Replacing the bridge path is more invasive than a local micro-optimization and must preserve batch-request and JSON-RPC error behavior.
