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

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated (complementary to reviewed/db/004-reader-rehashes-ledger-envelopes.md which addresses cross-request caching, not per-hash efficiency)

### Trace Summary

Traced the full path from `getTransactions` → `processTransactionsInLedger` → `newLedgerTransactionReader` → `storeTransactions` → `hashTransactionInEnvelopeWithID`. Confirmed the two-step marshal-then-hash waste exists. However, discovered a critical mechanism issue: `xdr.Marshal(w, v)` does NOT truly stream into the writer. When the value implements `encoding.BinaryMarshaler` (which all generated XDR types do), `Marshal` short-circuits by calling `MarshalBinary()` — which internally allocates a NEW `bytes.Buffer`, encodes into it, then copies the result to `w`. So passing a SHA-256 hasher to `xdr.Marshal` would NOT eliminate the internal allocation. The correct fix uses `payload.EncodeTo(xdr3.NewEncoder(hasher))` to bypass `MarshalBinary` entirely and stream XDR fields directly into the hasher.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:89` — calls `newLedgerTransactionReader` per ledger, confirmed hot path
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:storeTransactions:86-103` — iterates all envelopes, calls `hashTransactionInEnvelopeWithID` per envelope
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:hashTransactionInEnvelopeWithID:111-161` — confirmed: `buf.Reset()` → `xdr.Marshal(buf, payload)` → `hash.Hash(buf.Bytes())`
- `go-stellar-sdk@v0.4.0/xdr/xdr_generated.go:76-88` — `xdr.Marshal` checks `BinaryMarshaler` interface, short-circuits through `MarshalBinary()` which allocates internally
- `go-stellar-sdk@v0.4.0/xdr/xdr_generated.go:37404-37411` — `TransactionSignaturePayload.MarshalBinary()` creates a new `bytes.Buffer{}` and `xdr.NewEncoder(&b)` on every call
- `go-stellar-sdk@v0.4.0/xdr/xdr_generated.go:37369-37378` — `TransactionSignaturePayload.EncodeTo(e *xdr.Encoder)` — the true streaming path, writes fields directly through the encoder
- `go-stellar-sdk@v0.4.0/hash/main.go:9` — `hash.Hash` is just `sha256.Sum256(message)`
- `go-xdr@v0.0.0-20260312225820/xdr3/encode.go:775-777` — `xdr.NewEncoder(w io.Writer)` creates an encoder wrapping any writer

### Findings

The inefficiency is real and the fix is viable, but the proposed mechanism needs correction:

**Current flow per envelope (3 allocations, 2 data copies):**
1. `xdr.Marshal(buf, payload)` calls `payload.MarshalBinary()` → allocates internal `bytes.Buffer{}` + `xdr.Encoder` → encodes XDR into internal buffer
2. Internal buffer bytes are copied to external reusable `buf` via `buf.Write(b)`
3. `sha256.Sum256(buf.Bytes())` creates an internal hasher, reads bytes again for SHA-256

**Correct optimized flow (0 allocations in steady state, 1 data pass):**
1. Reuse a `crypto/sha256` hasher and `xdr3.Encoder` wrapping it across all envelopes
2. `hasher.Reset()` + `payload.EncodeTo(encoder)` — XDR fields stream directly into SHA-256
3. `hasher.Sum(result[:0])` — finalize, no intermediate buffer

The key insight is that `xdr.Marshal` cannot stream because it dispatches through `BinaryMarshaler`. You must call `EncodeTo` directly with a `go-xdr/xdr3.Encoder` wrapping the hasher.

For a dense ledger with 200 transactions (~500 bytes each), this eliminates ~200 buffer allocations + ~200KB of unnecessary copying per ledger. Across multi-ledger `getTransactions` responses, savings are proportional but remain a small fraction of total request cost (SHA-256 computation and LCM deserialization dominate).

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go` — `storeTransactions` and `hashTransactionInEnvelopeWithID`
- **Change description**:
  1. Import `"crypto/sha256"` and `xdr "github.com/stellar/go-xdr/xdr3"` (the latter as e.g. `goxdr`)
  2. In `storeTransactions`, replace `var buf bytes.Buffer` with a reusable `hasher := sha256.New()` and `encoder := goxdr.NewEncoder(hasher)`
  3. In `hashTransactionInEnvelopeWithID`, change signature to accept `hasher hash.Hash` and `encoder *goxdr.Encoder` instead of `buf *bytes.Buffer`
  4. Replace `buf.Reset(); xdr.Marshal(buf, payload); return hash.Hash(buf.Bytes())` with `hasher.Reset(); payload.EncodeTo(encoder); var h [32]byte; copy(h[:], hasher.Sum(nil)); return h, nil`
  5. Remove the `"github.com/stellar/go-stellar-sdk/hash"` import (no longer needed for `hash.Hash`)
- **Correctness check**: `ledger_transaction_reader_test.go` covers the hashing path; existing tests in `get_transactions_test.go` cover end-to-end correctness. The hash output must remain identical — `EncodeTo` produces the same XDR bytes as `MarshalBinary`, just without buffering.
- **Benchmark focus**: Allocation count and bytes allocated per `hashTransactionInEnvelopeWithID` call. CPU time per `storeTransactions` call on a 200-transaction ledger. Expect near-zero allocations vs ~3 per call currently, and a modest (~10-20%) reduction in per-hash CPU time.
