# H001: `resultMetaJson` and Top-Level Event Fields Serialize the Same Soroban Events Twice

**Date**: 2026-04-07
**Subsystem**: xdr2json
**Severity**: Medium
**Impact**: latency / CPU / allocations / memory bandwidth
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When a `getTransactions` JSON response includes both `resultMetaJson` and the
top-level `diagnosticEventsJson` / `events.{contractEventsJson,transactionEventsJson}`
fields, the conversion pipeline should serialize each logical event payload once
and reuse that work for every response field that needs it. A Soroban-heavy page
should not pay one full Rust parse/serialize while building `resultMetaJson` and
then a second xdr2json parse/serialize for the same event bodies.

## Mechanism

`transactionToJSON()` converts the full `TransactionMeta` blob to JSON, so
`resultMetaJson` already serializes the V3/V4 Soroban event collections embedded
inside transaction metadata. The same request then runs `jsonifySlice()` over
`tx.Events` and `BuildEventsJSONFromTransaction()` over `tx.ContractEvents` and
`tx.TransactionEvents`, which are extracted from that same metadata in
`db.ParseTransaction()` and re-marshaled to standalone event XDR before
`xdr_batch_to_json()` parses them again. A Rust entrypoint that parses
`TransactionMeta` once and returns both the full metadata JSON and the top-level
event JSON fragments from the same typed object would remove the second parse +
serialize pass.

## Trigger

1. Issue `getTransactions` with `xdrFormat=json`.
2. Use ledgers containing Soroban `TransactionMetaV3` / `TransactionMetaV4`
   records with many diagnostic, contract, or transaction events.
3. Profile CPU and allocation hot spots around `TransactionMeta` conversion and
   the follow-on event conversion calls. Expect the same event-heavy transactions
   to appear in both paths.

## Target Code

- `cmd/stellar-rpc/internal/methods/json.go:transactionToJSON:21-31` — serializes the full `TransactionMeta` into `resultMetaJson`.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:159-180` — the same response also fills `diagnosticEventsJson` and `events.*Json`.
- `cmd/stellar-rpc/internal/methods/get_transaction.go:BuildEventsJSONFromTransaction:143-152` — contract and transaction events are converted again through standalone batch calls.
- `cmd/stellar-rpc/internal/db/transaction.go:ParseTransaction/parseEvents:268-316` — event slices are extracted from the transaction metadata and re-marshaled to XDR bytes.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:xdr_to_json/xdr_batch_to_json:146-152,235-239` — both the full meta path and the event path reparse XDR and serialize fresh JSON.
- `gesserit/projects/stellar-core/summary-skills/protocol-curr/SKILL.md:157-160` — `TransactionMetaV3` / `TransactionMetaV4` already embed diagnostic, contract, and transaction event material.

## Evidence

The current handler sets `ResultMetaJSON` first and then independently populates
`DiagnosticEventsJSON` plus the nested `Events` object for the same transaction.
The protocol summary confirms that `TransactionMetaV3` already contains
`SorobanTransactionMeta` with `events` and `diagnosticEvents`, while V4 adds
per-operation events and `TransactionEvent`. The tests also build Soroban
transaction metadata with embedded events (`get_transaction_test.go:351-363`)
before the production path turns around and serializes those event families
separately.

## Anti-Evidence

The response schema intentionally exposes both `resultMetaJson` and the top-level
event fields, so the optimization cannot remove either field from the output. A
fix needs a new xdr2json ABI or a transaction-meta-specific helper that preserves
today's JSON shapes and event ordering exactly, which is materially more invasive
than a local batch tweak.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full JSON path in `getTransactions` from `processTransactionsInLedger` through `ParseTransaction`, `transactionToJSON`, `jsonifySlice`, and `BuildEventsJSONFromTransaction`. Confirmed the double work: `ParseTransaction` calls `ingestTx.GetTransactionEvents()` to extract typed events from `UnsafeMeta`, then `parseEvents()` marshals each event back to individual XDR byte slices; simultaneously `transactionToJSON` sends the full `TransactionMeta` XDR blob (which already contains these same events) to Rust for complete JSON serialization. The Rust side parses the events once as part of the full TransactionMeta and then again individually via `xdr_batch_to_json`. Both the Go-side re-marshal and the Rust-side re-parse are confirmed waste.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:149-186` — JSON branch calls `ParseTransaction`, then `transactionToJSON`, then `jsonifySlice` for DiagnosticEvents, then `BuildEventsJSONFromTransaction` for ContractEvents/TransactionEvents — 6 CGo crossings per transaction (3 for Result/Envelope/Meta + 3 for event batches)
- `cmd/stellar-rpc/internal/db/transaction.go:ParseTransaction:238-278` — marshals `UnsafeMeta` to XDR bytes (tx.Meta), then calls `GetTransactionEvents()` and `parseEvents()` to re-marshal each event individually
- `cmd/stellar-rpc/internal/db/transaction.go:parseEvents:281-317` — iterates DiagnosticEvents, TransactionEvents, and OperationEvents, calling `MarshalBinary()` on each to produce individual XDR byte slices
- `cmd/stellar-rpc/internal/methods/json.go:transactionToJSON:31` — `xdr2json.ConvertBytes(xdr.TransactionMeta{}, tx.Meta)` sends the full TransactionMeta XDR blob to Rust; Rust parses the entire structure including embedded events
- `cmd/stellar-rpc/internal/methods/json.go:jsonifySlice:56-58` — delegates to `ConvertBytesSlice`, sending individual event XDR blobs to Rust
- `cmd/stellar-rpc/internal/methods/get_transaction.go:BuildEventsJSONFromTransaction:143-156` — calls `jsonifySliceOfSlices` for ContractEvents and `jsonifySlice` for TransactionEvents
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:xdr_to_json:128-167` — generic Rust function: parses XDR via `read_xdr_to_end`, serializes via `serde_json::to_string`; when invoked with TransactionMeta, this includes all embedded events
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:xdr_batch_to_json:208-289` — batch variant: same parse+serialize per item; invoked separately for each event type batch

### Findings

**The double work is confirmed at three levels:**

1. **Go-side marshal waste**: `parseEvents()` calls `MarshalBinary()` on every DiagnosticEvent, TransactionEvent, and ContractEvent to produce individual XDR byte slices. These bytes are derived from the same `ingestTx.UnsafeMeta` that was already marshaled in full to `tx.Meta`. Per event: ~0.5-1μs for a 300-500 byte event.

2. **Rust-side double parse**: The full TransactionMeta XDR blob is parsed by `xdr_to_json` (including all embedded events as part of the structure). Then each individual event XDR blob is parsed again by `xdr_batch_to_json`. Per event: ~0.5-2μs.

3. **Rust-side double JSON serialize**: Events are serialized to JSON once as embedded fields within the TransactionMeta JSON, and again as standalone JSON objects. Per event: ~1-3μs.

**Estimated per-event overhead**: ~2-6μs (Go marshal + Rust re-parse + Rust re-serialize).

**Estimated per-request impact** (200-tx page, ~10 events/tx = 2000 events):
- Total event double-work: ~4-12ms
- Typical total request time for 200-tx JSON page: ~80-250ms
- Estimated savings: ~2-5%

**Severity downgrade from Medium to Low**: The double work is real and the fix is theoretically correct, but the practical impact is limited because:
- Events are a fraction of total TransactionMeta size (ledger entry changes typically dominate)
- Event conversions are already batched (minimizing CGo crossing overhead)
- The fix requires a fundamentally new TransactionMeta-specific Rust ABI, new FFI return types for multiple JSON fragments, and TransactionMeta version dispatch logic (V0/V1/V2/V3/V4) — a large architectural change for a small gain
- For edge-case event-heavy workloads (thousands of events per page), savings could approach 5-8%, but typical workloads will see 2-5%

**Contrast with XDR path**: The default (XDR) path in `processTransactionsInLedger` (lines 188-220) avoids this entirely — it encodes directly from typed Go structs via `enc.MarshalBase64` without going through `ParseTransaction`. This confirms the double work is specific to the JSON format path.

### PoC Guidance

- **Target code**: New Rust function in `cmd/stellar-rpc/lib/xdr2json/src/lib.rs` (e.g., `xdr_transaction_meta_to_json_with_events`); Go-side changes in `cmd/stellar-rpc/internal/xdr2json/conversion.go` (new wrapper), `cmd/stellar-rpc/internal/methods/json.go` (replace `transactionToJSON` + event calls with combined call), and `cmd/stellar-rpc/internal/db/transaction.go` (make `parseEvents` conditional or remove for JSON path)
- **Change description**: Add a TransactionMeta-specific Rust function that parses the XDR once, produces the full TransactionMeta JSON, and also extracts and individually serializes each DiagnosticEvent, ContractEvent, and TransactionEvent. Return all fragments via a new `TransactionMetaConversionResult` FFI struct. On the Go side, skip `parseEvents()` when the combined Rust function is used, and consume the event JSON fragments directly from the combined result.
- **Correctness check**: Existing tests in `get_transaction_test.go` and `get_transactions_test.go` verify JSON output shapes. Compare standalone event JSON from the combined function against the current separate-call output to ensure identical results. Run `make go-test`.
- **Benchmark focus**: Measure total wall-clock time and allocation bytes for `processTransactionsInLedger` on a 200-transaction JSON page with Soroban transactions containing 10+ events each. Expected: ~2-5% latency reduction, ~10-20% reduction in Rust-side CPU time for event-related work. The Go-side `parseEvents` marshal work should be fully eliminated.
