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
