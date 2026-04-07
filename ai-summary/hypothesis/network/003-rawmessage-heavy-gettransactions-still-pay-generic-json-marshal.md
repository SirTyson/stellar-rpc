# H003: RawMessage-heavy `getTransactions` replies still pay generic `encoding/json` marshaling in `directBridge`

**Date**: 2026-04-07
**Subsystem**: network
**Severity**: Medium
**Impact**: response serialization CPU / allocation
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

For `getTransactions` JSON responses, most of the heavy payload is already pre-rendered as `json.RawMessage` slices before control returns to the network layer. The bridge should serialize that response with a single append-style pass into the final JSON-RPC success envelope, instead of sending the entire `GetTransactionsResponse` back through generic reflective `encoding/json` marshaling and then copying the result again into an envelope buffer.

## Mechanism

`batchConvertTransactionsToJSON` has already converted envelope/result/meta/event blobs into `json.RawMessage` fields on each `protocol.TransactionInfo`, but `directBridge.serveInternal` still calls `json.Marshal(result)` on the full `protocol.GetTransactionsResponse`. That forces `encoding/json` to walk every `TransactionInfo` and nested `Events` field reflectively, build an intermediate `resultBytes` buffer, and then hand those bytes to `directBridgeSuccessResponse`, which copies them again into the JSON-RPC envelope. A `GetTransactionsResponse`-specific encoder (for example via `MarshalJSON` or an append-based bridge helper) could write scalars plus existing raw subdocuments straight into the final response buffer, eliminating the intermediate marshal buffer and much of the generic reflection overhead.

## Trigger

Issue large `getTransactions` requests with `xdrFormat=json`, `pagination.limit=200`, and ledgers dense with events. Compare baseline against a version that special-cases `protocol.GetTransactionsResponse` in the direct bridge or its protocol type marshaling, and measure response-path allocations plus p50/p95 latency.

## Target Code

- `cmd/stellar-rpc/internal/directbridge.go:108-112` — generic `json.Marshal(result)` still runs for successful replies before envelope construction
- `cmd/stellar-rpc/internal/directbridge.go:130-141` — the success envelope builder immediately copies that marshaled body into a second buffer
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/protocols/rpc/get_transactions.go:25-90` — `TransactionInfo`, `Events`, and `GetTransactionsResponse` are dominated by `json.RawMessage` fields and have no specialized marshaler
- `cmd/stellar-rpc/internal/methods/json.go:105-208` — the method layer already finishes the expensive XDR-to-JSON work before the bridge sees the response

## Evidence

The method code is already paying to produce raw JSON for the dominant per-transaction subdocuments, so the remaining bridge marshal is mostly packaging existing JSON with scalar metadata. The current success path nevertheless rebuilds the entire response through generic `encoding/json`, which creates a large intermediate buffer and repeats a structural walk over every transaction before the bridge's manual envelope copy even begins.

## Anti-Evidence

`encoding/json` is already reasonably efficient on `json.RawMessage`-heavy structs, so this is not a theoretical 2x win; the likely gain is in the 5-20% range only on the largest JSON pages. The custom encoder also has to preserve the exact current field order/escaping/`omitempty` behavior for both JSON and XDR-format responses, which raises implementation risk compared with a pure copy-elimination change.
