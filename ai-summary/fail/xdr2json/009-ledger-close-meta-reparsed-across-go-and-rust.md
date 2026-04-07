# H002: `getTransactions` Re-parses Raw `LedgerCloseMeta` Across Go and Rust Instead of Letting xdr2json Consume It Once

**Date**: 2026-04-07
**Subsystem**: xdr2json
**Severity**: High
**Impact**: latency / CPU / allocations / memory bandwidth
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

After `BatchGetLedgers()` returns raw `chunk.Lcm` bytes, the JSON path should
parse each needed ledger at most once before emitting transaction JSON. A
`getTransactions` page should not fully unmarshal `LedgerCloseMeta` into Go,
marshal per-transaction children back to XDR bytes, and then have xdr2json parse
those child blobs again.

## Mechanism

`getTransactionsByLedgerSequence()` fetches raw `chunk.Lcm`, immediately calls
`lcm.UnmarshalBinary()`, walks the typed ledger with `newLedgerTransactionReader()`,
and then `db.ParseTransaction()` marshals `TransactionResult`, `TransactionMeta`,
`TransactionEnvelope`, and every event back to standalone XDR byte slices. The
xdr2json bridge then calls `read_xdr_to_end()` on each of those blobs again in
Rust before `serde_json::to_string()`. A ledger-scoped xdr2json entrypoint that
accepts raw `LedgerCloseMeta` bytes and emits per-transaction JSON fragments
directly would collapse this decode -> encode -> decode pipeline into a single
Rust-side parse of the original DB bytes.

## Trigger

1. Issue `getTransactions` with `xdrFormat=json` and a large limit so one page
   spans many transactions and ledgers.
2. Use Soroban-heavy ledgers where `TransactionMeta` and event payloads are large.
3. Profile time and allocations across `lcm.UnmarshalBinary`, transaction/event
   `MarshalBinary`, and xdr2json's `read_xdr_to_end` calls. Expect all three stages
   to scale with the same raw ledger input.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:336-371` — receives raw `chunk.Lcm` and decodes it into `xdr.LedgerCloseMeta`.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:88-129` — walks typed transactions out of the decoded ledger.
- `cmd/stellar-rpc/internal/db/transaction.go:ParseTransaction/parseEvents:258-316` — marshals result/meta/envelope/events back to XDR bytes for JSON mode.
- `cmd/stellar-rpc/internal/db/transaction.go:Transaction:29-42` — the intermediate transport object stores those re-serialized byte slices.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:xdr_to_json/xdr_batch_to_json:143-152,232-239` — Rust parses every child XDR blob again before JSON serialization.
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:ConvertBytes/ConvertBytesSlice:65-132,135-167` — Go then performs many independent FFI conversions over the re-serialized children.

## Evidence

The handler already has the raw ledger bytes from SQLite, but the JSON path does
not hand those bytes to xdr2json. Instead it round-trips through a fully decoded
Go ledger and a second XDR byte representation per child object. Every JSON-mode
transaction pays this cost, including the three mandatory core fields and any
events extracted from the same ledger metadata.

## Anti-Evidence

This is a structural optimization, not a local patch: the Rust side would need a
ledger-aware FFI surface, and the Go side still has to preserve cursoring,
transaction hash/status derivation, and response ordering. End-to-end gains could
also be capped if the already-reviewed final response marshal path remains the
dominant bottleneck for very large pages.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated (related but distinct from fail/006 which addressed FFI input copies, and fail/005 which addressed core-field batching)
**Failed At**: reviewer

### Trace Summary

Traced the full getTransactions JSON pipeline: `getTransactionsByLedgerSequence` (get_transactions.go:362-364) calls `lcm.UnmarshalBinary(chunk.Lcm)` to produce typed `xdr.LedgerCloseMeta`, then `processTransactionsInLedger` (lines 88-233) creates a `ledgerTransactionReader` that walks the decoded LCM via `TransactionResultPair(i)`, `TxApplyProcessing(i)`, and `TransactionEnvelopes()` — all of which return typed Go objects, not raw byte slices. For JSON mode (line 151), `db.ParseTransaction` (transaction.go:238-278) calls `MarshalBinary()` on `ingestTx.Result.Result`, `ingestTx.UnsafeMeta`, and `ingestTx.Envelope` to re-serialize them as standalone XDR byte slices. These bytes are then passed through `transactionToJSON` → `xdr2json.ConvertBytes` → Rust `xdr_to_json` → `read_xdr_to_end`, where each component is decoded again from its standalone bytes before JSON serialization.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:362-364` — `lcm.UnmarshalBinary(chunk.Lcm)` fully decodes LCM from raw SQLite bytes into Go types
- `cmd/stellar-rpc/internal/methods/get_transactions.go:88-94` — `newLedgerTransactionReader(networkID, lcm)` walks the decoded LCM; `storeTransactions` hashes all envelopes to build tx-by-hash map
- `cmd/stellar-rpc/internal/methods/get_transactions.go:139-147` — Handler extracts `TransactionHash`, `ApplicationOrder`, `FeeBump`, `Ledger`, `LedgerCloseTime` from typed `ingestTx` — these metadata fields require the Go-side LCM decode REGARDLESS of format
- `cmd/stellar-rpc/internal/methods/get_transactions.go:149-186` — JSON format branch calls `db.ParseTransaction` then `transactionToJSON`, `jsonifySlice`, `BuildEventsJSONFromTransaction`
- `cmd/stellar-rpc/internal/methods/get_transactions.go:188-220` — XDR format branch ALSO calls `MarshalBase64` on the same typed objects — demonstrating the Go-side decode is always needed
- `cmd/stellar-rpc/internal/db/transaction.go:258-266` — `ParseTransaction` re-serializes `Result.Result`, `UnsafeMeta`, `Envelope` via `MarshalBinary()` (the "encode" step)
- `cmd/stellar-rpc/internal/db/transaction.go:280-316` — `parseEvents` re-serializes all DiagnosticEvents, TransactionEvents, ContractEvents via `MarshalBinary()`
- `cmd/stellar-rpc/internal/methods/json.go:12-36` — `transactionToJSON` sends re-serialized bytes through 3× `xdr2json.ConvertBytes`
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:63-73` — `Read()` returns `ingest.LedgerTransaction` with typed fields (`Envelope`, `Result`, `UnsafeMeta`) extracted from the decoded LCM via accessor methods
- `go-stellar-sdk@v0.4.0/xdr/ledger_close_meta.go:111-139` — `TransactionResultPair(i)` and `TxApplyProcessing(i)` return typed Go objects from the decoded LCM structure, NOT raw byte slices
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:127-167` — Rust `xdr_to_json` parses each standalone component blob via `read_xdr_to_end` then `serde_json::to_string`

### Why It Failed

The hypothesis overstates the achievable savings by claiming a "single Rust-side parse" would replace three steps. In reality:

1. **The Go-side LCM decode (step 1) is unavoidable.** The handler needs typed Go objects for transaction metadata — `TransactionHash` (line 141), `Successful()` (line 223), `IsFeeBump()` (line 143), `ApplicationOrder` (line 142) — regardless of output format. Even the XDR path uses these same typed fields (lines 188-220). No amount of Rust-side optimization removes this requirement.

2. **A Rust-side LCM parse does NOT eliminate the aggregate component decode work — it merely reorganizes it.** When Rust parses a full LCM via `read_xdr_to_end(TypeVariant::LedgerCloseMeta, ...)`, it must decode every `TransactionMeta`, `TransactionResultPair`, `TransactionEnvelope`, and event within the LCM — the same byte volume and same decode work as N individual `read_xdr_to_end` calls on standalone blobs. The per-component decode cost is inherent to the data, not to the call structure.

3. **The only truly eliminated cost is Go-side `MarshalBinary()` in `ParseTransaction`** — re-encoding typed Go objects into standalone XDR byte slices (~30-300μs/tx depending on component sizes). This is the cheapest step in the pipeline, dwarfed by both XDR deserialization and JSON serialization. For a page of 20 transactions, this saves ~600-6000μs — roughly 1-6% of total per-ledger processing time (~50-200ms).

4. **Prior benchmark evidence confirms sub-5% xdr2json optimizations are unmeasurable.** Fail/006 (eliminating two full input copies per conversion — a larger per-call saving than eliminating MarshalBinary alone) achieved only ~2.8% p50 improvement at 150 RPS and was REJECTED because the improvement fell within run-to-run variance. Fail/005 (batching core fields to eliminate per-call fixed overhead) yielded <1%. The MarshalBinary savings targeted here is smaller than fail/006's savings per conversion.

5. **Implementation complexity is disproportionate to expected gains.** The proposed change requires: (a) a new `xdr_lcm_transactions_to_json` Rust FFI function with complex return type (multiple per-transaction JSON fragments), (b) Rust-side LCM traversal logic including version handling (V0/V1/V2 branches), (c) passing network passphrase or pre-computed hash to Rust for transaction hash computation if Go's LCM parse is to be eliminated, (d) restructuring `processTransactionsInLedger` to receive JSON fragments from Rust instead of building them per-transaction, (e) coordinating pagination boundaries across the FFI boundary. This is an architectural redesign, not a targeted optimization.

### Lesson Learned

When evaluating "decode → encode → decode" pipeline hypotheses, the critical question is whether the proposed "single decode" truly eliminates work or merely moves it. In this case, the Rust-side LCM decode performs exactly the same component-level parsing that the individual Rust calls do — the data volume and algorithmic work are identical. The only genuinely eliminated step is the re-encoding (MarshalBinary), which is the cheapest step in the pipeline. Large structural optimizations that reorganize where parsing happens without reducing total parse work are unlikely to produce measurable throughput improvements, especially when prior benchmarks of simpler optimizations on the same path show sub-noise-floor results.
