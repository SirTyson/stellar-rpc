# H003: Rust raw-LCM extractor builds a full JSON DOM before emitting bytes

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Low
**Impact**: Rust allocation / heap retention / JSON serialization overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

The raw-LCM JSON extractor should serialize transaction fields directly into an output buffer, or at least into lightweight typed structs. It should not first materialize `serde_json::Value` trees for every result, meta, envelope, and event, then walk those trees again just to stringify the final page.

## Mechanism

`extract_events()` converts every event with `serde_json::to_value`, `extract_transactions_json()` converts result/meta/envelope the same way, wraps those values in `json!` objects, pushes them into a `Vec<serde_json::Value>`, and only then calls `serde_json::to_string` on the whole array. That creates a DOM-style allocation wave proportional to the full page, keeps all intermediate JSON values live until the end of the ledger conversion, and pays a second traversal to stringify them. Event-heavy or large-meta ledgers can therefore spend meaningful time in JSON-object materialization rather than in the XDR parse itself.

## Trigger

Call `getTransactions` with `format=json` on ledgers that contain large Soroban `TransactionMeta` payloads and many emitted events. A Rust-side allocation profile should show time and heap growth under `serde_json::to_value`, `serde_json::json!`, and the final `serde_json::to_string` pass.

## Target Code

- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:extract_events:431-472` — diagnostic, transaction, and contract events are eagerly converted into `Vec<serde_json::Value>`.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:extract_transactions_json:505-556` — result/meta/envelope are turned into `serde_json::Value`, then wrapped in `json!` objects and stored in a `Vec<serde_json::Value>`.

## Evidence

The Rust extractor explicitly builds `Vec<serde_json::Value>` collections for diagnostic events, transaction events, contract events, and the outer transaction array (`cmd/stellar-rpc/lib/xdr2json/src/lib.rs:433-468,505-556`). Every core field uses `serde_json::to_value(...)` before the final `serde_json::to_string(...)` (`cmd/stellar-rpc/lib/xdr2json/src/lib.rs:525-556`). That means the JSON output is represented twice in memory: first as a full DOM and then again as the serialized string returned to Go.

## Anti-Evidence

The extractor still must parse the XDR and produce exact JSON compatible with the existing path, so a streaming rewrite cannot remove the dominant XDR decode cost or change field ordering/shape. This is therefore a second-order optimization on top of the much larger architectural gain from bypassing the Go XDR round-trip.
