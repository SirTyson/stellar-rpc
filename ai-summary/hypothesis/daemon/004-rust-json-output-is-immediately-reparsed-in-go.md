# H004: Rust JSON Output Is Immediately Reparsed in Go

**Date**: 2026-04-07
**Subsystem**: daemon
**Severity**: Medium
**Impact**: redundant JSON encode/decode across the Rust-Go boundary
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once the Rust `xdr2json` path has already produced compact per-transaction JSON for a `getTransactions` page, the daemon should carry that JSON forward without immediately tokenizing it back into Go structs. The FFI boundary should not force a large JSON string to be serialized in Rust and then fully parsed again in Go before the response is written.

## Mechanism

`lcm_transactions_to_json` builds a `Vec<serde_json::Value>` and serializes the entire ledger's transaction array to a JSON string in Rust. Go then copies that byte buffer with `C.GoBytes` and immediately calls `json.Unmarshal` into `[]LCMTransactionJSON`, only to copy those `json.RawMessage` fields into `protocol.TransactionInfo` for later response serialization. Returning structured FFI results (or raw per-transaction JSON fragments plus scalar sidecar fields) would remove the large Rust string build plus the matching Go JSON parse from every JSON-mode page.

## Trigger

Benchmark `getTransactions` with `format=json` on large pages (`limit=50` or `200`) and compare the current Rust-stringify/Go-unmarshal path to a version where the FFI returns per-transaction JSON fragments and scalar metadata directly, so Go can populate the response without `json.Unmarshal`.

## Target Code

- `cmd/stellar-rpc/internal/xdr2json/conversion.go:45-78` — `LCMTransactionsToJSON` copies Rust's JSON string into Go and then `json.Unmarshal`s it into `[]LCMTransactionJSON`.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:283-324` — handler copies the parsed Rust payload into `protocol.TransactionInfo` structs field-by-field.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:356-381` — `lcm_transactions_to_json` returns one JSON string buffer for the whole ledger.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:505-556` — Rust builds a `Vec<serde_json::Value>` and serializes the full array with `serde_json::to_string`.

## Evidence

The Rust helper already has all scalar metadata and all per-transaction JSON fields in hand before it serializes the result array to a string. The Go wrapper then immediately pays `json.Unmarshal` on that same payload, which means the optimized raw-LCM path still includes a full JSON encode/decode round-trip between Rust and Go before any HTTP serialization even begins.

## Anti-Evidence

This change is more invasive than a local Go optimization because it alters the FFI contract, and some later response serialization work remains unless the daemon also adopts a raw-JSON response encoder. If JSON-mode traffic is rare compared to default XDR-mode traffic, overall endpoint impact may stay moderate.
