# H008: Default XDR Responses Reallocate Encoder Buffers for Every Transaction Field and Event

**Date**: 2026-04-06
**Subsystem**: methods
**Severity**: Medium
**Impact**: latency / CPU / allocations
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

The default XDR form of `getTransactions` should reuse encoding scratch space across a page, so emitting 50-200 transactions does not allocate fresh intermediate byte buffers for every result, envelope, meta, diagnostic event, transaction event, and contract event.

## Mechanism

The current path marshals every XDR object to a new `[]byte` in `ParseTransaction`, then allocates again when `EncodeToString` turns those bytes into base64 strings. The SDK already provides `xdr.EncodingBuffer` specifically to reuse XDR encoder and base64 scratch buffers across repeated encodes, but `getTransactions` never uses it. A format-specific XDR builder could encode directly from typed XDR values with one reusable buffer per request or per ledger, cutting a large allocation surface from the hot path.

## Trigger

1. Issue `getTransactions` in the default XDR format with large page sizes against dense ledgers, especially those with many events.
2. Capture allocation profiles and count `MarshalBinary`/`EncodeToString` churn across the page.
3. Compare against a prototype that uses `xdr.NewEncodingBuffer()` and format-specific XDR serialization instead of the `db.Transaction` byte-slice intermediate.

## Target Code

- `cmd/stellar-rpc/internal/db/transaction.go:258-319` — XDR fields and events are eagerly marshaled into newly allocated byte slices.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:193-200` — those byte slices are immediately base64-encoded into new strings for the response.
- `cmd/stellar-rpc/internal/methods/get_transaction.go:133-155` — shared event builders take the same allocation-heavy byte-slice path.
- `github.com/stellar/go-stellar-sdk/xdr/main.go:155-268` — `EncodingBuffer` exists to reuse encoder and base64 scratch buffers across repeated encodes.

## Evidence

`getTransactions` does not call `xdr.NewEncodingBuffer`, `MarshalBase64`, or `UnsafeMarshalBase64` anywhere, despite the SDK exposing a reusable encoder abstraction built for exactly this kind of repeated serialization. Every returned transaction currently pays one set of allocations to create XDR bytes and another set to create base64 strings from those bytes. Because XDR is the default response format, that cost applies even when clients avoid the heavier JSON path.

## Anti-Evidence

Sparse scans that are dominated by ledger fetches will amortize this less than dense pages that return many transactions from a few ledgers. If `MarshalBinary` itself dominates rather than buffer allocation, the win may stay in the lower end of the Medium range.
