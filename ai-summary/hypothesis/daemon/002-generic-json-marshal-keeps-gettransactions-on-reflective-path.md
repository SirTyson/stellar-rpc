# H002: Generic json.Marshal Keeps getTransactions on a Reflective Serialization Path

**Date**: 2026-04-07
**Subsystem**: daemon
**Severity**: Low
**Impact**: JSON serialization CPU
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

The hottest large-response RPC in the daemon should serialize its fixed response schema through a specialized path that appends known fields directly, especially once the inner transaction fields are already available as `string` and `json.RawMessage`. A `getTransactions` page with up to 200 `TransactionInfo` entries should not pay full generic `encoding/json` reflection and dispatch costs if a stable schema-specific encoder can emit the same bytes.

## Mechanism

`directBridge` always calls `json.Marshal(result)` on the method return value, so `getTransactions` pages go through the generic `encoding/json` machinery even though the response shape is fixed and known ahead of time. Because `protocol.GetTransactionsResponse` is a large nested struct of repeated `TransactionInfo`, strings, slices, and `json.RawMessage` fields, a bridge-side specialized encoder can avoid much of the per-field reflection, tag lookup, and dynamic dispatch that the generic marshal path repeats on every large page.

## Trigger

Benchmark `getTransactions` with large response pages in both `xdr` and `json` modes, then compare the current bridge to a version that type-switches on `protocol.GetTransactionsResponse` and appends the response JSON directly instead of calling generic `json.Marshal`.

## Target Code

- `cmd/stellar-rpc/internal/directbridge.go:108-112` — current bridge always marshals the returned value with generic `json.Marshal`.
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/protocols/rpc/get_transactions.go:42-90` — fixed `TransactionDetails`, `TransactionInfo`, and `GetTransactionsResponse` schema that a specialized encoder can target.

## Evidence

The bridge has no method-specific serialization fast path: every successful request converges on `json.Marshal(result)`. The `getTransactions` protocol types are static structs with a stable field order and many `json.RawMessage` members whose payloads are already serialized, which makes them a good fit for a custom append-based encoder implemented in daemon code rather than repeated reflective marshaling.

## Anti-Evidence

Any encoder still has to copy the actual string and raw JSON payload bytes into the final response buffer, so this cannot remove the dominant payload-size cost. The measurable win depends on `encoding/json` showing up in profiles once the larger DB/XDR/FFI inefficiencies are reduced.
