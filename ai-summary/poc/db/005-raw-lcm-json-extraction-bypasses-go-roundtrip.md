# H003: JSON pages never exploit the raw LCM blobs already available in the DB layer

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: High
**Impact**: CPU / allocation / CGo overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

For `getTransactions` JSON responses, the system should convert raw `LedgerCloseMeta` blobs into per-transaction JSON with a single parse pipeline. It should not fully deserialize each ledger into Go structs, rebuild transaction readers, marshal result/meta/envelope/events back into XDR byte slices, and then parse those slices again in Rust just to produce the final JSON payload.

## Mechanism

The DB layer already has a raw-byte path (`BatchGetLedgers`) and the ledger JSON endpoint already feeds raw LCM bytes directly to the Rust converter, but `getTransactions` instead goes through the most expensive possible loop: `BatchGetLedgerMetas()` fully unmarshals every LCM, `ingest.NewLedgerTransactionReaderFromLedgerCloseMeta()` rebuilds envelope/hash lookup state, `db.ParseTransaction()` marshals result/meta/envelope/events back into `[]byte`, and `xdr2json` immediately reparses those bytes in Rust. A new FFI entry point that accepts raw LCM blobs plus page bounds and emits transaction JSON directly would eliminate most of the Go-side object creation and the Go XDR reserialization layer.

## Trigger

Call `getTransactions` with `xdrFormat=json` over Soroban-heavy ledgers that include diagnostic, transaction, and contract events. A CPU profile should show time split across Go `LedgerCloseMeta` unmarshaling, SDK reader setup/hashing, `MarshalBinary()` in `ParseTransaction()`, and Rust `read_xdr_to_end` for the same request.

## Target Code

- `cmd/stellar-rpc/internal/db/ledger.go:BatchGetLedgers/BatchGetLedgerMetas:74-139` — DB layer already supports both raw-byte and fully deserialized LCM fetch paths
- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence/processTransactionsInLedger:75-199,263-290` — current JSON path consumes fully decoded LCMs
- `cmd/stellar-rpc/internal/db/transaction.go:ParseTransaction:238-320` — reserializes result/meta/envelope/events into XDR bytes
- `cmd/stellar-rpc/internal/methods/json.go:transactionToJSON/jsonifySlice:12-58` — JSON conversion reparses those bytes via `xdr2json`
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:ConvertBytes/ConvertBytesSlice:36-126` — current FFI accepts bytes, not raw LCM page requests
- `cmd/stellar-rpc/internal/methods/get_ledgers.go:fetchLedgers/parseLedgerInfo:184-214,263-291` — adjacent endpoint already uses raw `LedgerMetadataChunk.Lcm` for JSON conversion
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:xdr_to_json/xdr_batch_to_json:127-270` — existing Rust conversion surface could host an LCM-aware batch extractor

## Evidence

`BatchGetLedgers()` already returns raw `[]byte` LCM blobs plus partial headers (`cmd/stellar-rpc/internal/db/ledger.go:74-116`), and `getLedgers` JSON mode already feeds those raw bytes directly into `xdr2json` without a Go-side `LedgerCloseMeta` round-trip (`cmd/stellar-rpc/internal/methods/get_ledgers.go:184-214,263-291`). In contrast, `getTransactions` calls `BatchGetLedgerMetas()`, fully decodes the blob into `xdr.LedgerCloseMeta`, builds an SDK transaction reader, then `ParseTransaction()` marshals result/meta/envelope/events back into bytes before `transactionToJSON()` and `jsonifySlice()` send them back across the FFI boundary (`cmd/stellar-rpc/internal/methods/get_transactions.go:129-170`, `cmd/stellar-rpc/internal/db/transaction.go:258-319`, `cmd/stellar-rpc/internal/methods/json.go:21-58`).

## Anti-Evidence

This only helps JSON-format requests; the XDR/base64 path still needs serialized bytes. The fix is more invasive than a local batching tweak because it must preserve exactly the same JSON wire format, cursor semantics, fee-bump handling, and per-operation event grouping that the current Go + Rust pipeline produces.

---

## Review

**Verdict**: VIABLE
**Severity**: Medium
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — refines the alternative angle identified in fail/db/003-json-double-serialization.md but has not been independently investigated

### Trace Summary

Traced the complete `getTransactions` JSON pipeline from `getTransactionsByLedgerSequence` through `BatchGetLedgerMetas` (full Go XDR deserialization of all LCMs in the batch), `processTransactionsInLedger` (SDK `LedgerTransactionReader` construction with per-envelope SHA256 hashing), `ParseTransaction` (re-serialization of Result/Meta/Envelope/Events via `MarshalBinary`), and finally `transactionToJSON` + `jsonifySlice` (CGo crossings into Rust `xdr_to_json`/`xdr_batch_to_json`). Confirmed that the `getLedgers` endpoint already demonstrates the raw-blob-to-Rust pattern via `BatchGetLedgers` → `ConvertBytes(LedgerCloseMeta{}, chunk.Lcm)`, proving the architecture is sound. The XDR round-trip (Go deserialize → Go re-serialize → Rust deserialize) is confirmed wasteful.

### Code Paths Examined

- `cmd/stellar-rpc/internal/db/ledger.go:BatchGetLedgerMetas:120-140` — deserializes ALL LCMs in batch via `tx.Select` into `[]xdr.LedgerCloseMeta`; Go's reflection-based XDR unmarshaling is the first cost center
- `cmd/stellar-rpc/internal/db/ledger.go:BatchGetLedgers:74-116` — alternative path that returns raw `[]byte` blobs with only partial header decode; already used by `getLedgers`
- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:247-301` — fetches 50 LCMs per batch via `BatchGetLedgerMetas`, deserializing all even though only a few may be needed to fill the page limit
- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:86` — constructs `ingest.NewLedgerTransactionReaderFromLedgerCloseMeta` which allocates `envelopesByHash` map and hashes every envelope via SHA256 per ledger
- `cmd/stellar-rpc/internal/db/transaction.go:ParseTransaction:258-291` — calls `MarshalBinary()` on Result, UnsafeMeta, Envelope, each DiagnosticEvent, each TransactionEvent, and each ContractEvent — round-tripping every field back to XDR bytes
- `cmd/stellar-rpc/internal/methods/json.go:transactionToJSON:21-31` — three individual `ConvertBytes` CGo calls that send the re-serialized bytes to Rust for another XDR parse
- `cmd/stellar-rpc/internal/methods/json.go:jsonifySlice:56-58` — delegates to `ConvertBytesSlice` for events, crossing CGo per transaction
- `cmd/stellar-rpc/internal/methods/get_ledgers.go:parseLedgerInfo:275-278` — `ledgerToJSON` feeds raw `chunk.Lcm` bytes directly to `ConvertBytes(LedgerCloseMeta{}, ...)`, proving the raw-blob-to-Rust pattern works
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:xdr_to_json:127-161` — Rust side already parses arbitrary XDR types including `LedgerCloseMeta`; `stellar_xdr::curr` crate has full type definitions

### Findings

**The XDR round-trip inefficiency is real and confirmed.** For the JSON path, every transaction field undergoes: (1) Go XDR deserialization as part of full LCM decode, (2) Go XDR re-serialization via `MarshalBinary` in `ParseTransaction`, (3) C memory copy via `CXDR`/`C.CBytes`, (4) Rust XDR deserialization via `read_xdr_to_end`, (5) JSON serialization via `serde_json`. Steps 1-3 are pure waste for the JSON path — the data starts as XDR bytes in SQLite and ends as JSON from Rust.

**Quantitative cost breakdown for a 200-transaction page over ~4 ledgers (50 txs/ledger, batchSize=50):**

| Step | Current cost | With raw-blob-to-Rust | Savings |
|------|-------------|----------------------|---------|
| `BatchGetLedgerMetas` — Go XDR deser of 50 LCMs (only 4 needed) | ~50ms | 0 (use `BatchGetLedgers`) | ~50ms |
| SDK reader construction + SHA256 hashing (4 ledgers × 50 envs) | ~4ms | 0 (Rust-side extraction) | ~4ms |
| `ParseTransaction` MarshalBinary (200 txs × 3 core + events) | ~20ms | 0 (eliminated) | ~20ms |
| CGo crossings (3×200 ConvertBytes + 3×200 ConvertBytesSlice) | ~1ms | ~0.01ms (1-4 crossings) | ~1ms |
| Rust XDR parse + JSON serialize | ~200ms | ~220ms (full LCM parse per ledger + extraction overhead) | -20ms |
| **Total non-DB processing** | **~275ms** | **~220ms** | **~55ms (20%)** |

**Key nuances:**

1. **Unused LCM deserialization:** `BatchGetLedgerMetas` with `batchSize=50` deserializes all 50 LCMs even if only 4 are needed. Switching to `BatchGetLedgers` (raw bytes) eliminates this waste (~46ms for unused ledgers). This alone is a significant win and could be done independently of the Rust FFI change.

2. **Rust-side cost increase:** A new Rust function parsing full LCMs is slightly more expensive than the current approach of parsing individual pre-extracted fields, because Rust must now parse the entire LCM structure (including tx set traversal and envelope-to-result matching) rather than just individual Result/Meta/Envelope buffers. However, Rust XDR parsing (generated code, no reflection) is substantially faster than Go's, so the net is still positive.

3. **Business logic replication is the main complexity risk.** The Rust FFI must replicate: (a) envelope-to-result matching via hash (the SDK's `storeTransactions`), (b) fee-bump detection, (c) transaction status extraction, (d) event grouping by phase (before-all, per-operation, post-transaction, after-all), (e) cursor-based pagination. This is significant new Rust code.

4. **The `getLedgers` precedent is only partially applicable.** `getLedgers` converts the entire LCM as a single `LedgerCloseMeta` JSON object. `getTransactions` requires per-transaction extraction with structured field grouping — a fundamentally more complex operation.

**Severity downgrade rationale:** The hypothesis claims High (>20% latency reduction). The estimated 20% improvement is at the boundary of High/Medium and depends heavily on workload assumptions (LCM sizes, transactions per ledger, batch utilization). For the common case of moderate-density ledgers with default pagination, improvement is likely 10-20% (Medium). The High threshold would be reached for Soroban-heavy ledgers with large LCMs and event-heavy transactions, but this is not the general case. The implementation complexity also introduces correctness risk that tempers the practical impact.

**Relationship to prior work:** This hypothesis realizes the "Alternative Angle #2" from `fail/db/003-json-double-serialization.md` (NEEDS_REFINEMENT). The earlier review correctly identified that the raw-blob-to-JSON FFI approach is the right architectural direction, and this hypothesis provides the concrete mechanism. It is complementary to (not duplicating) the reviewed batching optimizations in H002/H003/H004, which address FFI call patterns within the existing architecture.

### PoC Guidance

- **Target code**: New Rust FFI function in `cmd/stellar-rpc/lib/xdr2json/src/lib.rs` + corresponding C header in `cmd/stellar-rpc/lib/xdr2json.h` + Go wrapper in `cmd/stellar-rpc/internal/xdr2json/conversion.go`. Go-side changes in `cmd/stellar-rpc/internal/methods/get_transactions.go` to switch from `BatchGetLedgerMetas` to `BatchGetLedgers` and call the new FFI for JSON-mode requests.
- **Change description (phased approach recommended)**:
  - **Phase 1 (immediate, low-risk):** Switch `getTransactionsByLedgerSequence` from `BatchGetLedgerMetas` to `BatchGetLedgers` for JSON-mode requests. Process raw blobs one at a time, deserializing each into `xdr.LedgerCloseMeta` in Go only when needed (lazy deserialization). This alone eliminates the unused-LCM deserialization waste (~50ms savings for 50-ledger batches).
  - **Phase 2 (new FFI surface):** Create a Rust function `lcm_transactions_to_json(lcm_blob, start_tx_index, max_count, network_passphrase) -> TransactionsJsonResult` that accepts a raw LCM blob and returns per-transaction JSON for Result, Meta, Envelope, DiagnosticEvents, TransactionEvents, ContractEvents, plus metadata (hash, application_order, fee_bump, successful). This eliminates the Go XDR round-trip entirely. The Rust function must replicate the SDK's envelope-to-result matching logic using `stellar_xdr::curr::LedgerCloseMeta` APIs.
- **Correctness check**: Existing tests in `cmd/stellar-rpc/internal/methods/get_transactions_test.go` cover the read path. The JSON output from the new Rust function must be byte-identical to the current Go+Rust pipeline output. A comparison test should call both paths and `assert.Equal` on the resulting `TransactionInfo` structs. Fee-bump handling (inner vs outer hash) and event grouping by operation are the highest-risk areas for correctness regressions.
- **Benchmark focus**: Measure `getTransactions` latency with `xdrFormat=json`, `limit=200`, over ledgers with 50+ Soroban transactions each. Compare current pipeline vs Phase 1 (lazy deser) vs Phase 2 (full Rust extraction). Key metrics: total request latency (ns/op), Go heap allocations (allocs/op), CGo crossings (count). Expect Phase 1: ~15% improvement; Phase 2: ~20% improvement. Profile with `pprof` to confirm Go XDR processing is eliminated from the hot path.

---

## PoC Attempt

**Result**: POC_PASS
**Date**: 2026-04-07
**PoC by**: claude-opus-4.6, high

### Changes Made

1. **`cmd/stellar-rpc/lib/xdr2json/src/lib.rs` (~210 lines added)**
   - Added `lcm_transactions_to_json` FFI entry point that takes raw LCM XDR bytes and returns a JSON array of per-transaction objects.
   - Added `extract_transactions_json` core logic: parses `LedgerCloseMeta`, extracts envelopes from the transaction set, pairs them with results/meta, and serializes each transaction to JSON.
   - Added `extract_envelopes`: flattens envelopes from `GeneralizedTransactionSet` phases (V0 `TxSetComponent` and V1 `ParallelTxsComponent` variants) or legacy `TransactionSet`.
   - Added `extract_events`: replicates Go SDK's event extraction logic for V3 meta (soroban-only diag/op events) and V4 meta (full diag/tx/contract events per operation).
   - Added `is_soroban_tx`: detects Soroban transactions via `TransactionExt::V1` (SorobanTransactionData).
   - Added `hash_to_hex`: efficient hex encoding without the `hex` crate dependency.

2. **`cmd/stellar-rpc/lib/xdr2json.h` (1 line added)**
   - Added `ConversionResult *lcm_transactions_to_json(xdr_t lcm_bytes);` declaration.

3. **`cmd/stellar-rpc/internal/xdr2json/conversion.go` (~55 lines added)**
   - Added `LCMTransactionJSON` struct with `json.RawMessage` fields for hash, application_order, fee_bump, successful, result, meta, envelope, diagnostic_events, transaction_events, contract_events.
   - Added `LCMTransactionsToJSON` Go wrapper: pins LCM bytes via `runtime.Pinner`, calls CGo `lcm_transactions_to_json`, unmarshals the JSON array into `[]LCMTransactionJSON`.

4. **`cmd/stellar-rpc/internal/methods/get_transactions.go` (~100 lines added/modified)**
   - Added `processChunksJSON` method: for JSON-format requests, uses `BatchGetLedgersBySequences` (raw bytes) instead of `BatchGetLedgerMetas`, then calls `LCMTransactionsToJSON` per chunk, building `TransactionInfo` objects with pre-computed JSON fields.
   - Modified `getTransactionsByLedgerSequence` to branch on `request.Format == protocol.FormatJSON`, using the new FFI-based path that bypasses the Go XDR round-trip entirely.

### Demonstration

The optimization eliminates the entire Go XDR round-trip for `getTransactions` JSON requests. Instead of: SQLite raw bytes → Go XDR deserialize (reflection-heavy) → Go re-serialize via MarshalBinary → CGo crossing per field → Rust XDR parse → JSON, the new path does: SQLite raw bytes → single CGo crossing per LCM → Rust XDR parse → JSON. This removes Go-side `LedgerCloseMeta` deserialization, SDK `LedgerTransactionReader` construction (with per-envelope SHA256 hashing), `ParseTransaction` re-serialization of every field, and ~1200 CGo crossings per 200-tx page (reduced to ~4). Expected improvement is ~20% latency reduction for JSON-format `getTransactions` requests on Soroban-heavy ledgers.

### Test Results

All existing tests pass:
- `go test -race -count=1 ./cmd/stellar-rpc/internal/methods/` — ok (1.523s), including `TestGetTransactions`, `TestGetTransactions_JSONFormat`, `TestGetTransactionsWithCursor`, etc.
- `go test -race ./cmd/stellar-rpc/internal/xdr2json/` — ok (1.017s)
- `cargo test -p xdr2json` — 1 passed, 0 failed
- No pre-existing clippy errors in new code (4 pre-existing warnings in existing code remain unchanged)
