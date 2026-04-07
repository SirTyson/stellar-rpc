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

---

## Review

**Verdict**: VIABLE
**Severity**: Medium
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full `getTransactions` JSON path from `processChunksJSON` (get_transactions.go:253-336) → `LCMTransactionsToJSON` (conversion.go:45-79) → `lcm_transactions_to_json` (lib.rs:356-382) → `extract_transactions_json` (lib.rs:477-557). Confirmed the DOM-building pattern: every transaction's `TransactionResult`, `TransactionMeta`, and `TransactionEnvelope` are each converted to `serde_json::Value` via `to_value()`, all events are converted to `Value` via `to_value()` in `extract_events`, everything is wrapped in a `json!({...})` macro (creating yet another `Value::Object`), and the entire `Vec<Value>` is serialized at the end via `to_string`. This is a two-pass pattern: one traversal to build the DOM, one to serialize it.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:253-336` — `processChunksJSON` calls `xdr2json.LCMTransactionsToJSON(chunk.Lcm)` for each ledger chunk
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:45-79` — `LCMTransactionsToJSON` sends raw LCM bytes to Rust, receives JSON, then `json.Unmarshal` re-parses the JSON array into `[]LCMTransactionJSON`
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:356-382` — `lcm_transactions_to_json` parses the LCM once via `read_xdr_to_end` and delegates to `extract_transactions_json`
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:477-503` — `extract_transactions_json` extracts `tx_set` and `processing` from LCM variants (V0/V1/V2)
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:506` — `let mut result_array: Vec<serde_json::Value>` — the DOM accumulator
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:526-531` — `serde_json::to_value(&result_pair.result)`, `to_value(meta)`, `to_value(envelope)` — each builds a `Value` tree with many small heap allocations (one per map entry, string key, array element)
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:433-472` — `extract_events` calls `serde_json::to_value(e)` for every DiagnosticEvent (V3/V4), TransactionEvent (V4), and ContractEvent (V3 events, V4 per-op events)
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:540-551` — `serde_json::json!({...})` creates a `Value::Object` with 10 heap-allocated string keys ("hash", "application_order", etc.) plus all the previously built Value sub-trees
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:556` — `serde_json::to_string(&result_array)` re-traverses the entire Value tree to produce the final JSON bytes

### Findings

**The inefficiency is confirmed.** The code performs two complete traversals of the transaction data:

1. **DOM construction pass**: `serde_json::to_value()` is called on every `TransactionResult`, `TransactionMeta`, `TransactionEnvelope`, and every event. Each call invokes the `Serialize` impl on the XDR type and outputs to a `serde_json::value::Serializer`, which allocates a `Value` tree. For a deeply nested `TransactionMeta` (which includes ledger entry changes, Soroban metadata, events), this creates hundreds to thousands of small heap allocations per transaction (one `String` per map key, one `Value` node per field/element).

2. **Serialization pass**: `serde_json::to_string(&result_array)` traverses the entire `Value` tree built in step 1, formatting each node into the output buffer.

With a typed `#[derive(Serialize)]` struct, only the serialization pass would execute — the `Serialize` impl would traverse the Rust XDR types directly and write JSON bytes, skipping the intermediate DOM entirely.

**Cost estimation per page (200 transactions):**

| Component | Per-tx DOM allocations | Notes |
|-----------|----------------------|-------|
| `to_value(meta)` | 500–2000 allocs | TransactionMeta is deeply nested; largest contributor |
| `to_value(result)` | 50–200 allocs | TransactionResult is smaller |
| `to_value(envelope)` | 200–500 allocs | TransactionEnvelope with ops/signatures |
| Events (`to_value` per event) | 20–100 allocs/event × N events | Scales with event count |
| `json!({...})` wrapper | 10 string key allocs | "hash", "application_order", etc. |

Conservative estimate: ~800–3000 allocations per transaction × 200 transactions = **160K–600K small heap allocations per page** purely for the DOM.

At ~30–50ns per malloc/free cycle (jemalloc), that's **5–30ms of allocator overhead** per page. The DOM traversal during `to_string` adds another pass over all these objects, contributing memory-bandwidth cost beyond the alloc/free churn.

For a typical 100–250ms JSON page request, the DOM overhead represents roughly **5–15%** of total latency. Event-heavy Soroban pages will be at the high end.

**No correctness concerns.** The XDR types already implement `Serialize` (that's how `to_value` works). A `#[derive(Serialize)]` wrapper struct referencing them would produce byte-identical JSON output if field names match. The `json!({...})` field names ("hash", "application_order", etc.) would become `#[serde(rename = "...")]` attributes on the struct fields.

**No overlap with prior hypotheses:**
- H002 (FFI bridge copies): addresses output-path copies, not DOM construction — orthogonal
- H011 (to_string vs to_vec): addresses serializer API shape — orthogonal
- H001-reviewed (double event serialization): addresses Go-side re-marshal and Rust re-parse of events from the old `processTransactionsInLedger` path — the current `processChunksJSON` already eliminates that double work, but the DOM overhead within `extract_transactions_json` remains

### PoC Guidance

- **Target code**:
  - `cmd/stellar-rpc/lib/xdr2json/src/lib.rs`: Define a `TransactionRecord<'a>` struct with `#[derive(serde::Serialize)]` that borrows all fields from the parsed LCM. Replace `extract_events` to return reference vectors (`Vec<&DiagnosticEvent>`, etc.) instead of `Vec<serde_json::Value>`. Replace the `result_array: Vec<Value>` + `json!({...})` pattern with `records: Vec<TransactionRecord>` and a single `serde_json::to_string(&records)` call.
  - No changes needed on the Go side or FFI boundary — the JSON output format is identical.

- **Change description**: Replace the two-pass DOM-then-serialize pattern in `extract_transactions_json` with a single-pass direct serialization via typed structs. This eliminates all `serde_json::to_value()` calls (and their hundreds of small heap allocations per transaction) and the `json!({...})` macro overhead, reducing the JSON generation path to a single `serde_json::to_string` traversal.

- **Struct sketch**:
  ```rust
  #[derive(serde::Serialize)]
  struct TransactionRecord<'a> {
      hash: String,
      application_order: i32,
      fee_bump: bool,
      successful: bool,
      result: &'a xdr::TransactionResult,
      meta: &'a xdr::TransactionMeta,
      envelope: &'a xdr::TransactionEnvelope,
      diagnostic_events: Vec<&'a xdr::DiagnosticEvent>,
      transaction_events: Vec<&'a xdr::TransactionEvent>,
      contract_events: Vec<Vec<&'a xdr::ContractEvent>>,
  }
  ```
  Note: verify exact event types for V3 vs V4 `TransactionMeta` variants. `extract_events` should return concrete reference types. For V4 `operations[i].events`, check whether these are `ContractEvent` references.

- **Correctness check**: All existing Go tests (`make go-test`) cover the JSON output shapes via `getTransactions` and `getTransaction` test cases. The Rust output bytes should be identical. Also run `cargo test` for the `xdr2json` crate.

- **Benchmark focus**: Measure per-page allocation count and total allocated bytes for `getTransactions` with `format=json` on Soroban-heavy ledgers. Expect 160K–600K fewer allocations per page. Latency improvement target: 5–15% for event-heavy pages. Also measure peak RSS reduction (the DOM + final JSON string held simultaneously vs. only the final JSON string).
