# H002: JSON getTransactions Re-encodes Typed XDR Before Rust Parses It Again

**Date**: 2026-04-07
**Subsystem**: daemon
**Severity**: Medium
**Impact**: redundant serialization / JSON-mode CPU
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When `getTransactions` is serving `format=json`, the handler should convert each transaction's already-decoded XDR structures to JSON without first re-serializing them back into XDR byte slices and then reparsing those bytes in Rust. Each transaction component should be parsed once per request, not decode in Go, encode to bytes, then decode again in the FFI layer.

## Mechanism

After `LedgerCloseMeta` has already been unmarshaled and `newLedgerTransactionReader` has produced typed `ingestTx` values, the JSON path calls `db.ParseTransaction`, which `MarshalBinary()`-encodes `TransactionResult`, `TransactionMeta`, `TransactionEnvelope`, and every event into fresh byte slices. `transactionToJSON`, `jsonifySlice`, and `BuildEventsJSONFromTransaction` then hand those bytes to `xdr2json`, whose Rust side calls `xdr::Type::read_xdr_to_end(...)` before `serde_json::to_string(...)`. That means JSON mode pays a full Go encode plus a second Rust decode for the same logical data on every transaction and event.

## Trigger

Benchmark `getTransactions` with `format=json`, `limit=200`, and event-heavy ledgers, then compare the current path to a Rust-side page extractor that consumes raw `LedgerCloseMeta` bytes (or another typed one-pass representation) and emits all per-transaction JSON fields without going through `db.ParseTransaction`'s `MarshalBinary` round-trip.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:149-187` — JSON mode routes every transaction through `db.ParseTransaction`, `transactionToJSON`, and event JSON helpers.
- `cmd/stellar-rpc/internal/db/transaction.go:ParseTransaction:238-317` — re-encodes result, meta, envelope, diagnostic events, transaction events, and contract events into `[]byte`.
- `cmd/stellar-rpc/internal/methods/json.go:transactionToJSON/jsonifySlice/jsonifySliceOfSlices:12-37,56-90` — feeds those byte slices into `xdr2json`.
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:ConvertBytes/ConvertBytesSlice:37-44,65-132` — Go FFI entrypoints for byte-based conversion.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:xdr_to_json/xdr_batch_to_json:128-153,208-239` — reparses each XDR buffer with `read_xdr_to_end` before JSON serialization.

## Evidence

The code path is explicit: `processTransactionsInLedger` first builds typed `ingestTx`, then `db.ParseTransaction` marshals those typed values back into byte slices, and `xdr2json` immediately parses the byte slices back into Rust XDR types. The existing batch FFI helper only amortizes call overhead; it does not remove this encode/decode round-trip.

## Anti-Evidence

This only affects `format=json`; the default base64/XDR mode does not use `db.ParseTransaction` or `xdr2json`. The current FFI path already avoids input-side `C.CBytes` copies, so the remaining win comes from eliminating repeated XDR encode/decode work rather than from large C-heap copy savings.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: FAIL — duplicate of `ai-summary/fail/xdr2json/009-ledger-close-meta-reparsed-across-go-and-rust.md`
**Failed At**: reviewer

### Trace Summary

This hypothesis describes the identical decode→encode→decode pipeline as the previously rejected xdr2json/009 hypothesis: Go decodes `LedgerCloseMeta` from raw DB bytes, `db.ParseTransaction` re-encodes components via `MarshalBinary()`, and Rust's xdr2json re-decodes those bytes via `read_xdr_to_end` before JSON serialization. The mechanism, target code paths, and proposed fix (a Rust-side pass that consumes raw LCM bytes directly) are substantively equivalent.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:149-187` — JSON format branch calls `db.ParseTransaction` then `transactionToJSON` and event helpers
- `cmd/stellar-rpc/internal/db/transaction.go:262-301` — `ParseTransaction` calls `MarshalBinary()` on Result, UnsafeMeta, Envelope (the re-encode step)
- `cmd/stellar-rpc/internal/db/transaction.go:305-340` — `parseEvents` re-serializes all DiagnosticEvents, TransactionEvents, ContractEvents via `MarshalBinary()`
- `cmd/stellar-rpc/internal/methods/json.go:12-36` — `transactionToJSON` sends re-serialized bytes through 3× `xdr2json.ConvertBytes`
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:128-167` — Rust `xdr_to_json` parses each standalone component blob via `read_xdr_to_end`

### Why It Failed

This is a duplicate of the thoroughly investigated `xdr2json/009-ledger-close-meta-reparsed-across-go-and-rust.md`, which was rejected NOT_VIABLE with detailed analysis. The prior review established:

1. **The Go-side LCM decode is unavoidable** — the handler needs typed Go objects for `TransactionHash`, `Successful()`, `IsFeeBump()`, `ApplicationOrder` regardless of output format. Even the XDR path uses these same typed fields.

2. **A Rust-side LCM parse does not eliminate aggregate decode work** — it merely reorganizes where parsing happens. Rust must still decode every `TransactionMeta`, `TransactionResultPair`, `TransactionEnvelope`, and event within the LCM — the same total byte volume and algorithmic work as N individual `read_xdr_to_end` calls on standalone blobs.

3. **The only truly eliminated cost is `MarshalBinary()` in `ParseTransaction`** — re-encoding typed Go objects into standalone XDR byte slices (~30-300μs/tx). This is the cheapest step in the pipeline, dwarfed by XDR deserialization and JSON serialization.

4. **Prior benchmark evidence from fail/xdr2json/006 (eliminating two full input copies per conversion — a larger per-call saving) achieved only ~2.8% p50 improvement at 150 RPS**, which fell within run-to-run variance and was rejected as unmeasurable.

The current hypothesis reframes the same finding from the daemon subsystem perspective but does not introduce any new mechanism, code path, or mitigation strategy beyond what xdr2json/009 already evaluated.

### Lesson Learned

Cross-subsystem hypotheses should be checked against prior findings in all relevant subsystem fail directories. The decode→encode→decode pattern in the getTransactions JSON pipeline has been thoroughly evaluated: the re-encode step (MarshalBinary) is the only eliminable work, and it represents <5% of total per-transaction processing time — below the measurable threshold established by prior xdr2json benchmarks.
