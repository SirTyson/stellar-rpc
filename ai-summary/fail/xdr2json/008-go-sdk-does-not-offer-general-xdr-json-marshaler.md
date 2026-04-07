# H008: Replacing xdr2json With Go-Side `json.Marshal` on XDR Types Is Not Currently Viable

**Date**: 2026-04-07
**Subsystem**: xdr2json
**Severity**: High
**Impact**: latency / CPU / allocations
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

If the Go Stellar XDR package already provided general `MarshalJSON`
implementations for `TransactionMeta`, `TransactionEnvelope`, `TransactionResult`,
`DiagnosticEvent`, `ContractEvent`, and `TransactionEvent`, `getTransactions`
could potentially bypass xdr2json and serialize directly from the typed Go
objects it already holds on the JSON path.

## Mechanism

The JSON path already owns typed `xdr.LedgerCloseMeta`, transaction result,
envelope, and event values before `db.ParseTransaction()` turns them back into
XDR bytes, so direct Go-side JSON marshaling looks like a tempting way to remove
the Rust FFI boundary entirely. That shortcut is only viable if the Go SDK
exposes the same general-purpose JSON encoders for the hot XDR union types that
xdr2json currently handles.

## Trigger

1. Inspect the current `go-stellar-sdk` XDR package for `MarshalJSON` support on
   the hot `getTransactions` XDR types.
2. Compare what exists there with the types that `transactionToJSON()`,
   `jsonifySlice()`, and `BuildEventsJSONFromTransaction()` send through xdr2json.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:149-180` — the JSON path already has typed Go values before it calls xdr2json.
- `cmd/stellar-rpc/internal/methods/json.go:transactionToJSON/jsonifySlice:12-36,56-57` — current callers assume xdr2json is the only JSON serializer for these XDR types.
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/xdr/json.go:1-178` — the package contains only narrow custom JSON helpers, not a general serializer for the hot union types.
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/protocols/rpc/get_transactions.go:25-68` — the RPC response schema expects raw JSON payloads for result/meta/envelope/events.

## Evidence

The dependency does expose a small `xdr/json.go`, so the idea is not obviously
dead on arrival. The current handler also already owns typed Go values for the
transaction and event structures before it re-marshals them to XDR bytes.

## Anti-Evidence

The inspected Go SDK code only provides custom JSON logic for narrow helper types
such as `iso8601Time` and `ClaimPredicate`, plus unrelated protocol-layer types.
There is no drop-in `MarshalJSON` implementation for the hot
`TransactionMeta` / `TransactionEnvelope` / `TransactionResult` / event union
types used by `getTransactions`, so swapping out xdr2json here would require
building new serialization logic and risking schema divergence rather than
reusing an existing library path.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The alternate serializer I was hoping to reuse does not exist in the current
Go dependency set. Without general-purpose `MarshalJSON` support for the hot XDR
types, "just use Go JSON here" is not a small substitution for xdr2json; it is a
new serializer project with different correctness risks.

### Lesson Learned

Before treating xdr2json as optional, verify that the Go side actually has a
schema-compatible JSON encoder for the same XDR unions. A few helper
`MarshalJSON` methods in the SDK are not evidence of a complete replacement path
for `getTransactions`.
