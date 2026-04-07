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

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

`processChunksJSON` (get_transactions.go:257) passes raw LCM bytes to `xdr2json.LCMTransactionsToJSON` (conversion.go:45), which crosses CGo into `lcm_transactions_to_json` (lib.rs:356). Rust parses the LCM via `ReadXdr`, then calls `extract_transactions_json` (lib.rs:477), which builds `serde_json::Value` trees for every result, meta, envelope, and event via `to_value()`, assembles them into `json!({...})` objects, collects all into `Vec<serde_json::Value>`, and finally calls `serde_json::to_string()` for the second traversal. The existing `xdr_to_json` function (lib.rs:153) already demonstrates the efficient pattern: `serde_json::to_string(&t)` serializes directly from XDR structs without intermediate Value allocation.

### Code Paths Examined

- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:extract_transactions_json:477-557` — builds `Vec<serde_json::Value>` with `to_value()` for result (line 526), meta (line 528), envelope (line 530), then wraps in `json!({})` (line 540), and finally serializes entire array with `to_string` (line 556). Two full traversals.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:extract_events:433-472` — converts every diagnostic event, transaction event, and contract event into `serde_json::Value` via `to_value()` (lines 445, 449, 458, 461, 465). All held in memory until the outer `to_string` call.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:xdr_to_json:140-156` — the per-type converter already uses the efficient pattern: `serde_json::to_string(&t)` on line 153, serializing directly from XDR struct to JSON string without any intermediate Value DOM.
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:LCMTransactionsToJSON:45-79` — Go side calls into Rust, then `json.Unmarshal` parses the returned JSON string (line 74). The Rust-side Value DOM overhead adds to the total time before this Go-side parse even begins.

### Findings

The inefficiency is confirmed and real:

1. **Double traversal**: `to_value()` traverses each XDR struct creating heap-allocated `Value` nodes (BTreeMap entries for objects, Vec entries for arrays, String allocations for every key and string value). Then `to_string()` traverses the entire Value tree a second time to produce the JSON string. The direct `to_string(&struct)` pattern already used in `xdr_to_json` would collapse both traversals into one.

2. **Peak memory amplification**: All Value trees for all transactions in a ledger are held simultaneously (the `result_array` Vec keeps them alive until line 556). For a Soroban-heavy ledger with large TransactionMeta, this means thousands of small heap objects (one per JSON node) alive concurrently, rather than the streaming pattern where only the output buffer grows.

3. **Precedent within the same file**: The `xdr_to_json` function at line 153 already demonstrates `serde_json::to_string(&t)` directly on XDR types. The stellar_xdr crate's types implement `Serialize`, confirming that direct serialization produces the same JSON format.

4. **Fix is straightforward**: Define a `#[derive(serde::Serialize)]` struct holding references to the XDR types, and call `serde_json::to_string()` on the Vec of those structs. This eliminates all intermediate Value allocations while producing identical JSON content. Field ordering may change (struct declaration order vs. BTreeMap alphabetical order), which tests should verify.

5. **Impact estimate**: The XDR parse (`ReadXdr`) remains the dominant cost. The Value DOM overhead is estimated at 10-20% of Rust-side time (allocation + second traversal), translating to <5% of end-to-end getTransactions latency. This is measurable on event-heavy ledgers but not a game-changer.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/lib/xdr2json/src/lib.rs` — functions `extract_transactions_json` (lines 477-557) and `extract_events` (lines 433-472)
- **Change description**: (1) Add `use serde::Serialize;` and define a `#[derive(Serialize)]` struct (e.g., `TxJsonOutput<'a>`) with fields referencing the XDR types (`&'a xdr::TransactionResult`, `&'a xdr::TransactionMeta`, `&'a xdr::TransactionEnvelope`, etc.) plus owned scalar fields (`hash: String`, `application_order: i32`, `fee_bump: bool`, `successful: bool`). (2) Modify `extract_events` to return references to XDR event types (e.g., `Vec<&'a xdr::DiagnosticEvent>`) instead of `Vec<serde_json::Value>`. (3) Replace the `json!({})` assembly + `to_string` with `serde_json::to_string(&result_array)` where `result_array: Vec<TxJsonOutput>`. (4) Verify JSON field ordering compatibility — the `json!` macro sorts keys alphabetically via BTreeMap; `#[derive(Serialize)]` uses declaration order. Use `#[serde(rename)]` if needed or declare fields alphabetically.
- **Correctness check**: `make go-test` covers the JSON path via `getTransactions` tests. Also run `cargo test` in `cmd/stellar-rpc/lib/xdr2json/` — the existing `test_lcm_transactions_to_json_*` tests validate round-trip correctness.
- **Benchmark focus**: Rust-side allocation count and serialization time for `lcm_transactions_to_json` on Soroban-heavy ledgers. A Rust benchmark comparing the Value-based vs. direct serialization approach on a representative LCM blob would isolate the improvement. End-to-end, expect <5% latency improvement on JSON-format getTransactions at high RPS with event-heavy workloads.
