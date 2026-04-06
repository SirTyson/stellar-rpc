# H007: JSON getTransactions Materializes Temporary XDR Blobs Before Converting Them Right Back to JSON

**Date**: 2026-04-06
**Subsystem**: methods
**Severity**: Medium
**Impact**: latency / CPU / allocations
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

For `format=json`, `getTransactions` should serialize directly from the typed XDR values already present in `ingest.LedgerTransaction`, instead of first marshaling every result, envelope, meta, and event into temporary `[]byte` buffers that are immediately fed into the XDR-to-JSON FFI path.

## Mechanism

`db.ParseTransaction` eagerly calls `MarshalBinary` for the transaction result, envelope, meta, diagnostic events, transaction events, and contract events, storing all of them in a `db.Transaction`. The JSON branch then passes those temporary byte slices into `transactionToJSON`, `jsonifySlice`, and `BuildEventsJSONFromTransaction`, which copy them again into C memory and reparse them in Rust. Even if the FFI fan-out were batched, this Go-side marshal-to-bytes stage would still create avoidable allocations and data copies on every returned transaction.

## Trigger

1. Issue `getTransactions` with `format=json` against ledgers containing many Soroban transactions and events.
2. Measure allocations and CPU time spent in `MarshalBinary` before any `xdr2json` call runs.
3. Compare against a prototype that builds JSON directly from `ingestTx`/typed XDR objects instead of the byte-oriented `db.Transaction` intermediate.

## Target Code

- `cmd/stellar-rpc/internal/db/transaction.go:238-289` — `ParseTransaction` eagerly marshals every transaction field and event into `[]byte`.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:162-191` — JSON path consumes those temporary XDR byte slices immediately.
- `cmd/stellar-rpc/internal/methods/json.go:12-37` — `transactionToJSON` converts result, envelope, and meta from the temporary byte slices.
- `cmd/stellar-rpc/internal/methods/json.go:56-80` — event JSON conversion walks the temporary byte slices again.
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:61-80` — each conversion copies the already-marshaled bytes into C memory.

## Evidence

The hot path currently serializes typed XDR into Go heap buffers solely so the next step can deserialize those bytes back into structured JSON. `ParseTransaction` is format-agnostic, so this work happens even though the JSON branch never uses the XDR byte slices as response values. That is a separate inefficiency from H004's many-CGo-calls problem: batching FFI calls would still leave this redundant Go-side marshaling in place.

## Anti-Evidence

This only affects `format=json`; the XDR path genuinely needs serialized bytes. The benefit is smaller for transactions with few or no events because the temporary-byte explosion grows with event count.
