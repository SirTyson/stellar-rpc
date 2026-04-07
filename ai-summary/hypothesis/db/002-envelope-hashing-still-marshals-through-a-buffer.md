# H002: Envelope hashing still marshals through a reusable buffer and then hashes a second pass

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Low
**Impact**: CPU / allocation overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

The custom ledger reader should hash each transaction signature payload in a single streaming pass. It should not serialize the payload into a `bytes.Buffer` and then immediately read those bytes again just to feed SHA-256.

## Mechanism

`hashTransactionInEnvelopeWithID()` resets a reusable `bytes.Buffer`, calls `xdr.Marshal(buf, payload)`, and then computes `hash.Hash(buf.Bytes())`. That means every envelope pays one full write of the encoded payload into memory and then a second full read of the same bytes for hashing. Because `xdr.Marshal` already writes to an `io.Writer`, the function could instead stream directly into a reusable SHA-256 writer (`hash.Hash`) and eliminate the staging buffer contents and second memory pass without changing the join key or response semantics.

## Trigger

Run `getTransactions` against dense ledgers that force `newLedgerTransactionReader()` to hash many envelopes per request, especially low-limit polling that repeatedly revisits the same hot ledgers. CPU profiles should show time under `hashTransactionInEnvelopeWithID`, `bytes.Buffer.Write`, and `hash.Hash(buf.Bytes())` while reader setup happens before any transaction is returned.

## Target Code

- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:storeTransactions:86-103` — hashes every envelope eagerly on the request path
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:hashTransactionInEnvelopeWithID:111-160` — buffer-backed XDR marshal followed by a second-pass SHA-256

## Evidence

The local optimized reader already precomputes `networkID` and reuses one `bytes.Buffer`, but it still does a two-step marshal-then-hash flow: `xdr.Marshal(buf, payload)` at lines 155-157 and `hash.Hash(buf.Bytes())` at line 160 (`cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go`). The XDR layer's `Marshal` API accepts any `io.Writer`, so a streaming hash sink is mechanically compatible with the current code shape.

## Anti-Evidence

The bigger win on this path is still to avoid rehashing whole ledgers across requests at all; this hypothesis only reduces the cost of each required hash. XDR encoding work remains even after removing the staging buffer, so the expected gain is low and most visible on dense, hash-heavy workloads.
