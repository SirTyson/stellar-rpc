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
