# H001: `getTransactions` can return a pre-encoded result instead of a giant response struct

**Date**: 2026-04-07
**Subsystem**: network
**Severity**: Medium
**Impact**: serialization CPU / peak heap
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once `getTransactions` has already produced JSON-form transaction payloads, the network path should not need to walk and marshal a huge `protocol.GetTransactionsResponse` object again just to hand bytes to JSON-RPC. The hot path should encode the final result exactly once, then reuse those bytes when the JSON-RPC server and logging wrapper need the method result.

## Mechanism

`processTransactionsInLedger()` already converts the expensive parts of each transaction into `json.RawMessage` values (`resultJson`, `envelopeJson`, `resultMetaJson`, diagnostic events, and event payloads), but `getTransactionsByLedgerSequence()` still returns a large Go struct slice that `jrpc2.Server.invoke` later marshals in full at the network boundary. A `getTransactions`-specific `jrpc2.Handler` that assembles the final result object bytes once and returns `json.RawMessage` would remove that post-handler marshal pass, and it would also make `logResponse()`'s current success-path marshal degrade to a cheaper raw-message copy instead of another struct walk.

## Trigger

Request `getTransactions` with `format=json` and `pagination.limit` near the configured maximum against ledgers with populated events. Compare current behavior with a version that assembles the final result JSON inside the handler and returns `json.RawMessage` directly, measuring CPU in `encoding/json`, bytes allocated per request, and peak heap under concurrency.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:(transactionsRPCHandler).processTransactionsInLedger:152-207` — already produces per-transaction JSON fragments that could be appended into a final result buffer
- `cmd/stellar-rpc/internal/methods/get_transactions.go:(transactionsRPCHandler).getTransactionsByLedgerSequence:267-327` — materializes the full `[]protocol.TransactionInfo` response graph before returning
- `cmd/stellar-rpc/internal/methods/get_transactions.go:NewGetTransactionsHandler:330-341` — currently returns the generic wrapped handler instead of a `getTransactions`-specific exact-signature handler
- `cmd/stellar-rpc/internal/jsonrpc.go:logResponse:124-141` — re-marshals every successful handler result before the bridge sees it
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/server.go:(*Server).invoke:377-395` — marshals the returned handler value with `json.Marshal(v)` after the method completes

## Evidence

`GetTransactionsResponse` is unusually friendly to pre-encoding: the expensive nested blobs are already `json.RawMessage` in `protocol.TransactionDetails`, so the handler is doing the hard JSON conversion work before the network layer runs. The remaining server-side marshal is therefore mostly a large top-level array/object walk plus copies of already-serialized fragments, and it happens after the handler has already allocated the full response graph in memory.

## Anti-Evidence

This does not remove the separate local-bridge response round-trip that was already identified in reviewed H002, so it cannot eliminate all response-side overhead by itself. The XDR path and small pages will benefit less, and building the final result bytes inside the handler is more invasive than ordinary wrapper tweaks.
