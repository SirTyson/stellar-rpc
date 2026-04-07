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

- `cmd/stellar-rpc/internal/methods/get_transactions.go` (lines 70-262):
  - Added `enc *xdr.EncodingBuffer` parameter to `processTransactionsInLedger`.
  - Created `xdr.NewEncodingBuffer()` in `getTransactionsByLedgerSequence` (line 309), passed to all `processTransactionsInLedger` calls so the buffer is reused across all ledgers and transactions in a single request.
  - For the default (XDR) format path: bypassed `db.ParseTransaction` entirely. Metadata fields (TransactionHash, ApplicationOrder, FeeBump, Successful, Ledger) are extracted directly from `ingestTx`. XDR fields are encoded directly from typed values using `enc.MarshalBase64()` — `&ingestTx.Result.Result`, `&ingestTx.UnsafeMeta`, `&ingestTx.Envelope`, plus each diagnostic/transaction/contract event.
  - For the JSON format path: kept `db.ParseTransaction` since `xdr2json.ConvertBytes` requires `[]byte` inputs.
  - Added `buildEventsXDRDirect()` helper that encodes `TransactionEvents` and `ContractEvents` directly from typed XDR values using the shared `EncodingBuffer`.
  - Removed unused `encoding/base64` import (no longer needed in this file).

### Demonstration

The optimization eliminates all intermediate `[]byte` allocations in the `getTransactions` XDR response path. Instead of marshaling each XDR value to a fresh `[]byte` via `MarshalBinary()` and then allocating another `string` via `base64.StdEncoding.EncodeToString()`, the code now uses `xdr.EncodingBuffer.MarshalBase64()` which goes directly from typed XDR values to base64 strings while reusing the encoder's internal byte buffer across all transactions and events in the page. For a page of 200 transactions with 5 events each, this eliminates ~3200 intermediate allocations (1600 `[]byte` + 1600 redundant buffer allocations within `MarshalBinary`).

### Test Results

All 12 test packages in `cmd/stellar-rpc/internal/...` pass, including the `methods` package which contains dedicated `getTransactions` tests covering default limits, custom limits, cursor pagination, JSON format, missing ledgers, and edge cases. Tests run with `-race` flag enabled, confirming no data races.

---

## Final Review — Needs Revision

**Date**: 2026-04-07
**Final review by**: gpt-5.4, high

### What Needs Fixing

The code change itself is plausible and passed an independent `make -j8 build-stellar-rpc` plus `make go-test`, but the performance claim did not survive adversarial benchmarking. My first paired sweep suggested a win at 200 RPS and fewer errors near saturation, yet targeted rechecks contradicted that result:

- Initial sweep at **200 RPS**: baseline `p50=71.295ms`, optimized `p50=48.191ms`
- Recheck at **200 RPS**: baseline `p50=107.519ms`, optimized `p50=273.663ms`
- Initial sweep at **350 RPS**: baseline `42` errors, optimized `1` error
- Recheck at **340 RPS**: baseline `1` error, optimized `0` errors

Those numbers are too unstable to support a confirmed latency reduction or a stable zero-error throughput ceiling increase attributable to the optimization rather than run-to-run variance on the live futurenet-backed setup.

### Revision Instructions

1. Re-run the benchmark with **multiple paired repetitions per RPS level** (at least 3 baseline/optimized pairs), alternating the order or restarting between pairs so one side does not always benefit from warmer caches or better network conditions.
2. Report **medians or another aggregate across repetitions**, not a single run, and only claim an improvement if it holds on repeat at the same RPS or as a higher zero-error ceiling.
3. If the latency/RPS win still does not hold up, reframe this as an **Informational allocation/GC optimization** and support it with allocation profiling rather than throughput claims.
4. Keep the scope on `getTransactions` XDR only; the code path and implementation are in scope, but the current benchmark evidence is not strong enough for confirmation.

### Checks Passed So Far

- The claimed inefficiency is real: `getTransactions` previously marshaled XDR to fresh `[]byte` values and then re-encoded those bytes to base64 strings for every transaction/event field.
- The implementation targets that exact waste by using `xdr.EncodingBuffer` on the XDR response path only.
- The change appears behavior-safe: the encoder buffer is request-local, no shared mutable state was introduced, and the repository build plus Go tests passed independently.
