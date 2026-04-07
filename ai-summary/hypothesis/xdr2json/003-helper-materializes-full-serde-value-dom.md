# H003: `extract_transactions_json` Still Builds a Full `serde_json::Value` DOM Before Writing Bytes

**Date**: 2026-04-07
**Subsystem**: xdr2json
**Severity**: Medium
**Impact**: latency / CPU / allocations / memory bandwidth
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

The ledger-scoped helper should stream transaction JSON directly into its output
buffer, or at least serialize typed structs without first materializing a complete
`serde_json::Value` tree for every transaction and event. Large Soroban ledgers
should not hold both a DOM representation and the final JSON bytes in memory at
the same time.

## Mechanism

`extract_events()` maps every event into `serde_json::Value`, and
`extract_transactions_json()` separately converts `result`, `meta`, and `envelope`
into `Value`, wraps them in `serde_json::json!({...})`, pushes those objects into
`result_array: Vec<Value>`, and only then runs `serde_json::to_string(&result_array)`.
That creates an allocation-heavy intermediate tree proportional to the full ledger
response before the final byte buffer exists. A streaming `serde_json::Serializer`
or typed transaction record written directly to a `Vec<u8>` would remove the
intermediate DOM and cut both allocator churn and peak memory.

## Trigger

1. Issue `getTransactions` with `format=json` for ledgers containing large
   `TransactionMetaV3/V4` values and many events.
2. Profile Rust allocations inside `serde_json::to_value`, `json!`, and the final
   `serde_json::to_string`.
3. Compare against a prototype that writes the ledger array directly with a
   streaming serializer and no intermediate `Value` tree.

## Target Code

- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:433-473` — `extract_events()` converts every event into `serde_json::Value`.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:505-556` — `extract_transactions_json()` builds `result_array: Vec<serde_json::Value>` and serializes it at the end.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:525-553` — `result`, `meta`, `envelope`, and event collections are all wrapped into `json!` objects before output.

## Evidence

The current helper constructs `result_json`, `meta_json`, `envelope_json`,
`diag_events`, `tx_events`, and `contract_events` as `Value`s first, then embeds
them in another `Value` per transaction, then serializes the whole vector. That
is a classic DOM-building pattern, not a streaming serialization path.

## Anti-Evidence

The final JSON bytes still have to be generated, so this cannot eliminate the
dominant serialization cost itself. Small ledgers with tiny payloads may not gain
enough from removing the intermediate tree to justify the implementation
complexity.
