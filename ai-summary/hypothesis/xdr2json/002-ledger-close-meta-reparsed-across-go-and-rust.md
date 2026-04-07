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
