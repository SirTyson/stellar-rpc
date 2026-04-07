# H003: Default getTransactions Builds Large Base64 Strings Before the Bridge Can Write JSON

**Date**: 2026-04-07
**Subsystem**: daemon
**Severity**: Medium
**Impact**: allocation churn / XDR-mode latency
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

For the default XDR/base64 response format, `getTransactions` should encode XDR fields directly into the outgoing JSON payload using reusable buffers. It should not first allocate immutable Go strings for every envelope, result, meta, and event only for `directBridge` to walk the struct and copy those same bytes into a second response buffer moments later.

## Mechanism

The XDR path fills `protocol.TransactionInfo` with `string` and `[]string` fields by repeatedly calling `EncodingBuffer.MarshalBase64`. That helper uses `UnsafeMarshalBase64` and then does `return string(b)`, forcing a copy from the reusable scratch buffer into a new heap string for every field and event. `getTransactionsByLedgerSequence` retains those strings in the `txns` slice until `directBridge` later calls `json.Marshal(result)`, which copies them again into the final JSON response bytes. A getTransactions-specific writer that appends quoted output from `UnsafeMarshalBase64` directly into the response buffer could remove the intermediate string copies and reduce peak heap size for large default-format pages.

## Trigger

Benchmark the default `getTransactions` format (`format=""` / base64) with `limit=200`, especially on ledgers carrying many diagnostic or contract events, then compare allocation profiles and tail latency against a path that writes base64 directly from `UnsafeMarshalBase64` into a specialized `getTransactions` encoder instead of populating `TransactionInfo` string fields.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:188-227` — allocates base64 strings for result/meta/envelope plus per-event string slices before appending to `txns`.
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/xdr/main.go:UnsafeMarshalBase64/MarshalBase64:197-205,263-269` — reusable scratch buffer exists, but `MarshalBase64` copies it into a new `string`.
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/protocols/rpc/get_transactions.go:TransactionDetails/GetTransactionsResponse:42-90` — the API shape stores the encoded payload as strings inside a large response struct.
- `cmd/stellar-rpc/internal/directbridge.go:serveInternal:108-123` — later marshals the entire populated response struct into a separate JSON byte buffer.

## Evidence

The SDK helper explicitly shows the extra copy: `MarshalBase64` calls `UnsafeMarshalBase64` and then converts the returned `[]byte` to `string`. The handler then stores many of those strings in `txns`, while `directBridge` always performs a later full `json.Marshal(result)` pass over the same response content.

## Anti-Evidence

Base64 encoding itself is still required, so this does not eliminate the core encode cost the way a raw-byte fast path would. It also overlaps with broader bridge-side encoder work, so the measurable win depends on how much of the page's total time is currently spent on string allocation and GC rather than on XDR marshaling itself.
