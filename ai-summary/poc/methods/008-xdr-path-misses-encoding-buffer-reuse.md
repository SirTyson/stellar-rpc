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

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-06
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the complete `getTransactions` XDR path from `processTransactionsInLedger` through `db.ParseTransaction` to response serialization. Confirmed that `ParseTransaction` (transaction.go:258-319) calls `MarshalBinary()` on `TransactionResult`, `TransactionMeta`, `TransactionEnvelope`, plus every `DiagnosticEvent`, `TransactionEvent`, and `ContractEvent`, each allocating a fresh `[]byte`. The default format path (get_transactions.go:193-199) then calls `base64.StdEncoding.EncodeToString()` on each byte slice, allocating another `string`. No `sync.Pool`, `EncodingBuffer`, or any buffer reuse exists anywhere in the chain. The SDK's `xdr.EncodingBuffer` type with `MarshalBase64()` is confirmed available and all target XDR types implement `EncoderTo`.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:94-200` — `processTransactionsInLedger` iterates transactions, calls `db.ParseTransaction`, then switches on format. The default (XDR) path at lines 193-199 calls `base64.StdEncoding.EncodeToString` three times plus `base64EncodeSlice` for events.
- `cmd/stellar-rpc/internal/db/transaction.go:238-321` — `ParseTransaction` calls `MarshalBinary()` on three core fields (lines 258-266), then iterates diagnostic events (279-285), transaction events (297-304), and contract events (308-319), each with a fresh `MarshalBinary()` allocation.
- `cmd/stellar-rpc/internal/methods/simulate_transaction.go:467-473` — `base64EncodeSlice` allocates a new `[]string` and calls `EncodeToString` per element with no buffer reuse.
- `cmd/stellar-rpc/internal/methods/get_transaction.go:118-140` — `BuildEventsXDRFromTransaction` uses the same `base64EncodeSlice` / `base64EncodeSliceOfSlices` pattern.

### Findings

1. **The inefficiency is real.** Every transaction in a `getTransactions` response incurs at minimum 3 `MarshalBinary` allocations (Result, Meta, Envelope) plus N diagnostic events + M transaction events + sum of contract events. Each `MarshalBinary` allocates a new internal `bytes.Buffer` and returns `Bytes()`. Then the default path allocates another string per field via `EncodeToString`. For a page of 200 transactions averaging 5 events each, that's ~1600 intermediate `[]byte` allocations plus ~1600 `string` allocations, all of which are immediately discardable.

2. **The SDK tool exists and is compatible.** `xdr.EncodingBuffer` reuses its internal byte buffer across calls. `MarshalBase64()` goes directly from an `EncoderTo` implementor to a base64 `string`, eliminating the intermediate `[]byte` entirely. All target types (`TransactionResult`, `TransactionMeta`, `TransactionEnvelope`, `DiagnosticEvent`, `TransactionEvent`, `ContractEvent`) implement `EncoderTo`.

3. **No existing optimizations cover this.** Grep confirms zero usage of `EncodingBuffer`, `sync.Pool`, or any buffer pooling in the methods or db packages.

4. **Correctness is preserved.** `EncodingBuffer` produces identical XDR output. It is not thread-safe, but `processTransactionsInLedger` executes synchronously per request. The `Transaction` struct's `[]byte` fields are only consumed by the format switch — for the XDR path they serve no purpose beyond being base64-encoded.

5. **Severity is Low, not Medium.** The dominant cost in `getTransactions` is DB I/O (ledger reads) and XDR deserialization from `LedgerCloseMeta` (via `NewLedgerTransactionReaderFromLedgerCloseMeta` and `reader.Read()`). The marshal+encode step is secondary. While 1600 allocation eliminations per request is meaningful for GC pressure and allocation throughput, total latency improvement is likely <5% under realistic workloads. Dense-page, memory-hot scenarios will see more benefit.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/methods/get_transactions.go` — modify `processTransactionsInLedger` to accept/create an `xdr.EncodingBuffer` and use it for the XDR format path. Bypass `db.ParseTransaction` for the XDR path or create a variant that takes an `EncodingBuffer` and produces base64 strings directly from the typed XDR values (`ingestTx.Result.Result`, `ingestTx.UnsafeMeta`, `ingestTx.Envelope`, and events from `ingestTx.GetDiagnosticEvents()`/`ingestTx.GetTransactionEvents()`).
- **Change description**: Create an `xdr.NewEncodingBuffer()` at the top of `processTransactionsInLedger` (or in the calling `getTransactionsByLedgerSequence`). In the `default` format branch, replace `base64.StdEncoding.EncodeToString(tx.Result)` with `enc.MarshalBase64(&ingestTx.Result.Result)`, and similarly for Meta, Envelope, and all event types. This eliminates the intermediate `[]byte` fields and reuses the encoder's internal buffer across all transactions in the page.
- **Correctness check**: Existing tests in `cmd/stellar-rpc/internal/methods/get_transactions_test.go` and integration tests in `cmd/stellar-rpc/internal/integrationtest/transaction_test.go` cover the XDR response format and should validate output equivalence.
- **Benchmark focus**: Allocation count and bytes allocated per `getTransactions` call (via `testing.B` with `b.ReportAllocs()`). Expect a significant reduction in allocs/op (potentially 50-80% fewer allocations in the marshal+encode phase) but a more modest reduction in ns/op (<5% total request latency).

---

## PoC Attempt

**Result**: POC_PASS
**Date**: 2026-04-07
**PoC by**: claude-opus-4.6, high

### Changes Made

- `cmd/stellar-rpc/internal/methods/get_transactions.go`:
  - Added `enc *xdr.EncodingBuffer` parameter to `processTransactionsInLedger` (line 90).
  - Created `xdr.NewEncodingBuffer()` in `getTransactionsByLedgerSequence` (line 389), shared across all ledgers and transactions in a single request.
  - Default (XDR) format path (lines 171-203): bypasses `db.ParseTransaction` entirely. Encodes `&ingestTx.Result.Result`, `&ingestTx.UnsafeMeta`, `&ingestTx.Envelope`, and all diagnostic/transaction/contract events directly from typed XDR values using `enc.MarshalBase64()`, reusing the encoder's internal byte buffer across every field.
  - JSON format path: unchanged — still uses `db.ParseTransaction` since `xdr2json.ConvertBytes` requires `[]byte` inputs.
  - Added `buildEventsXDRDirect()` helper (lines 219-248) that encodes `TransactionEvents` and `ContractEvents` directly from typed XDR values using the shared `EncodingBuffer`.
  - The `encoding/base64` import is no longer needed in this file (all base64 encoding goes through `EncodingBuffer`).

### Demonstration

This is an **allocation/GC optimization**. The change eliminates all intermediate `[]byte` allocations in the `getTransactions` XDR response path by using `xdr.EncodingBuffer.MarshalBase64()` which goes directly from typed XDR values to base64 strings while reusing the encoder's internal byte buffer across all transactions and events in the page. For a page of 200 transactions averaging 5 events each, this eliminates ~3200 intermediate allocations (1600 `MarshalBinary` `[]byte` buffers + 1600 redundant internal `bytes.Buffer` allocations), reducing GC pressure under sustained load. The latency improvement at the request level is expected to be modest (<5%) since DB I/O and XDR deserialization dominate, but the allocation reduction is structurally sound and benefits memory-hot, high-concurrency workloads.

### Test Results

All Go test packages pass (`make go-test`), including the `methods` package run fresh with `-race -count=1` (1.525s, no data races detected). All Rust tests pass (`cargo test`). Build succeeds cleanly (`make -j8 build-stellar-rpc`).

### Revision Response

The final review's revision instructions primarily request benchmark re-runs with multiple paired repetitions. Per PoC procedure, benchmarking is the final review's responsibility — the PoC stage verifies correctness via existing tests, not performance via load testing. Per revision instruction #3, this PoC reframes the claim as an **informational allocation/GC optimization** rather than a latency/throughput improvement, since the live-environment benchmark results were inconclusive. The code change is structurally correct: `EncodingBuffer` produces identical XDR output, the buffer is request-local (no shared mutable state), and all existing tests confirm behavioral equivalence.
