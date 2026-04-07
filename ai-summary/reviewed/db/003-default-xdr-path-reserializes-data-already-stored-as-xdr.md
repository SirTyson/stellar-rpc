# H003: Default XDR responses reserialize substructures that already exist in the stored ledger blob

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Medium
**Impact**: CPU / allocation overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

For the default `xdr` response format, `getTransactions` should reuse the XDR bytes that are already persisted inside `ledger_close_meta` as much as possible. Returning base64 XDR should not require full Go reserialization of result, meta, envelope, and event substructures when the source data is already an XDR blob in SQLite.

## Mechanism

The current default-format path discards the raw blob by calling `BatchGetLedgerMetas()`, then reconstructs typed transaction/result/meta/event values and immediately serializes them again through `xdr.EncodingBuffer.MarshalBase64()`. A raw-blob extractor that works from `BatchGetLedgers()` and application-order bounds could base64-encode the original XDR subsegments directly (or via an offset-aware helper), eliminating a large amount of Go marshaling on the default format path.

## Trigger

Call `getTransactions` in the default `xdr` format with high limits over Soroban-heavy ledgers containing large `TransactionMeta` payloads and many events. A profile should show substantial time in `MarshalBase64()` for `Result`, `UnsafeMeta`, `Envelope`, diagnostic events, transaction events, and contract events even though the ledger entered the system as raw XDR bytes.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:188-219` — default-format branch reserializes result, meta, envelope, and events to base64
- `cmd/stellar-rpc/internal/methods/get_transactions.go:238-264` — event XDR is rebuilt from typed values per transaction
- `cmd/stellar-rpc/internal/db/ledger.go:68-116` — DB layer already has a raw-byte ledger path via `BatchGetLedgers`
- `cmd/stellar-rpc/internal/db/ledger.go:118-140` — current `getTransactions` path chooses fully deserialized `LedgerCloseMeta` values instead
- `cmd/stellar-rpc/internal/methods/get_ledgers.go:273-289` — adjacent endpoint already base64-encodes the original raw ledger bytes directly

## Evidence

The default branch in `processTransactionsInLedger()` calls `enc.MarshalBase64()` on every returned transaction field and event (`cmd/stellar-rpc/internal/methods/get_transactions.go:188-219,238-264`). At the same time, the DB layer already exposes `BatchGetLedgers()` with raw `[]byte` LCM data (`cmd/stellar-rpc/internal/db/ledger.go:68-116`), and `getLedgers` uses those raw bytes directly for its XDR response (`cmd/stellar-rpc/internal/methods/get_ledgers.go:283-289`). The wasted work here is specific to `getTransactions`: it chooses `BatchGetLedgerMetas()` and then rebuilds XDR that started life as the same stored blob (`cmd/stellar-rpc/internal/db/ledger.go:118-140`).

## Anti-Evidence

The raw ledger blob does not currently expose offsets for per-transaction result/meta/envelope/event slices, and transaction envelopes are not stored in processing order, so the extractor logic is more complex than a local loop rewrite. This optimization helps the default XDR format only; JSON responses still need separate conversion work.

---

## Review

**Verdict**: VIABLE
**Severity**: Medium
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

The `getTransactions` XDR path performs a full XDR→Go→XDR round-trip on every transaction field. `fetchLedgerMetas` calls `BatchGetLedgerMetas` (ledger.go:120-140), which deserializes raw SQLite blobs into `[]xdr.LedgerCloseMeta` via Go's XDR Scanner. Then `processTransactionsInLedger` (get_transactions.go:77-235) creates a `ledgerTransactionReader` that builds an envelope-by-hash map (requiring full envelope deserialization and hashing), reads each transaction as an `ingest.LedgerTransaction`, and re-serializes `Result.Result`, `UnsafeMeta`, `Envelope`, diagnostic events, transaction events, and contract events back to XDR via `enc.MarshalBase64()`. The output bytes are byte-identical to the corresponding sub-segments in the original SQLite blob.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:312-395` — `fetchLedgerMetas` calls `BatchGetLedgerMetas` at line 371, deserializing all LCMs in the requested range into Go structs
- `cmd/stellar-rpc/internal/db/ledger.go:120-140` — `BatchGetLedgerMetas` issues `SELECT meta` and deserializes directly into `[]xdr.LedgerCloseMeta` via the SDK's XDR Scanner
- `cmd/stellar-rpc/internal/db/ledger.go:74-116` — `BatchGetLedgers` provides an alternative raw-byte path, returning `LedgerMetadataChunk` with `Lcm []byte` and only partially deserializing the header
- `cmd/stellar-rpc/internal/methods/get_transactions.go:77-235` — `processTransactionsInLedger` creates a `ledgerTransactionReader`, iterates transactions, and in the default case (lines 189-220) calls `enc.MarshalBase64()` on Result, UnsafeMeta, Envelope, and events
- `cmd/stellar-rpc/internal/methods/get_transactions.go:239-266` — `buildEventsXDRDirect` serializes each TransactionEvent and ContractEvent via `enc.MarshalBase64()`
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:28-104` — `newLedgerTransactionReader` deserializes all envelopes via `TransactionEnvelopes()`, hashes each one, and builds `envelopesByHash` map; `Read()` returns sub-struct references from the deserialized LCM
- `go-stellar-sdk/xdr/main.go:188-206` — `UnsafeMarshalBase64` calls `EncodeTo` (full struct tree walk) then base64-encodes the buffer
- `go-stellar-sdk/xdr/ledger_close_meta.go:62-150` — `TransactionEnvelopes()` collects envelopes from TxSet phases (NOT in processing order); `TxApplyProcessing(i)` and `TransactionResultPair(i)` index directly into `txProcessing`
- `cmd/stellar-rpc/internal/methods/get_ledgers.go:282-289` — `getLedgers` XDR path directly base64-encodes `ledger.Lcm` raw bytes, demonstrating the pattern this hypothesis would extend to per-transaction granularity

### Findings

**The round-trip is confirmed and wasteful.** The chain is: raw XDR bytes (SQLite) → full Go struct deserialization (XDR Scanner in `BatchGetLedgerMetas`) → struct field access → `EncodeTo` tree walk + XDR serialization → base64 encoding. XDR is a canonical encoding, so the output of re-serialization is byte-identical to the corresponding sub-segments in the original blob. The deserialization and re-serialization are both O(n) in data size.

**The cost is proportional to `TransactionMeta` size.** For Soroban transactions, `TransactionMeta` (accessed via `UnsafeMeta`) contains contract data changes, events, and return values — often the largest single per-transaction structure (many KB). The `EncodeTo` call must walk the entire nested Go struct tree to produce the same bytes that were just parsed.

**Six `MarshalBase64` calls per transaction on the XDR path.** Each transaction produces calls for: `Result.Result`, `UnsafeMeta`, `Envelope`, each `DiagnosticEvent`, each `TransactionEvent`, and each per-operation `ContractEvent`. For Soroban transactions with many events, the aggregate cost scales.

**`BatchGetLedgers` already demonstrates the raw-byte pattern.** The DB layer has `BatchGetLedgers` (ledger.go:74-116) returning raw `[]byte` plus a partially-deserialized header. The `getLedgers` endpoint uses this at line 288 to directly base64-encode the raw bytes.

**Envelope ordering adds complexity but is not a blocker.** Envelopes in `GeneralizedTransactionSet` are organized by phases/clusters, not processing order. Matching requires hash computation (already done in `storeTransactions`). A raw-byte approach would still need to deserialize envelopes for hashing, but could avoid deserializing `TransactionMeta`, `TransactionResult`, and events entirely.

**Some metadata fields require partial deserialization.** The code reads `TransactionHash` (for hex string), `Index` (for application order), `Envelope.IsFeeBump()` (for flag), and `Result.Successful()` (for status). These are small fields compared to the bulk meta/events data.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/methods/get_transactions.go` (fetchLedgerMetas + processTransactionsInLedger), `cmd/stellar-rpc/internal/db/ledger.go` (new method or extended BatchGetLedgers)
- **Change description**: For the XDR format path, switch from `BatchGetLedgerMetas` (full deserialization) to a hybrid approach: use `BatchGetLedgers` to get raw bytes, then write an XDR streaming parser that walks the LCM blob to record byte offsets of each `TransactionResultMeta`'s result, txApplyProcessing, and feeProcessing sub-segments. For envelopes, still deserialize and hash them (needed for processing-order matching), but for result/meta/events, base64-encode directly from the raw byte offsets. An alternative simpler approach: continue deserializing fully but add a `MarshalBinary` cache that records the XDR bytes produced for each sub-struct during the first serialization, then base64-encodes from cache.
- **Correctness check**: Existing `TestGetTransactions*` tests in `cmd/stellar-rpc/internal/methods/` cover the XDR output format. Run these to verify byte-identical output. Also run `make go-test` for full regression.
- **Benchmark focus**: Measure CPU time and allocations in the `MarshalBase64` calls per transaction. The `BenchmarkGetTransactions` benchmark (if it exists) or a custom benchmark hitting getTransactions with Soroban-heavy ledgers should show reduced CPU in the serialization phase. Target: 5-15% reduction in overall getTransactions latency for Soroban-heavy workloads with the raw-byte approach; the simpler cache approach may yield 3-8%.
