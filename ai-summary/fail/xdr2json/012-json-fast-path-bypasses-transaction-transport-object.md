# H001: `getTransactions` JSON Mode Still Repackages Each Transaction Through `db.Transaction`

**Date**: 2026-04-07
**Subsystem**: xdr2json
**Severity**: Medium
**Impact**: latency / CPU / allocations / GC
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

The JSON `getTransactions` path should derive transaction metadata once and convert
the typed `ingest.LedgerTransaction` directly into the final JSON fragments. It
should not first repack the same transaction into a transient `db.Transaction`
containing freshly allocated XDR byte slices that are consumed immediately by
xdr2json and then discarded.

## Mechanism

`processTransactionsInLedger()` already extracts `txHash`, `applicationOrder`,
`feeBump`, `ledger`, and `createdAt` directly from `ingestTx`, then the JSON
branch calls `db.ParseTransaction()` which recomputes the same metadata and
materializes `Result`, `Meta`, `Envelope`, and event XDR byte slices into a
transport struct. A JSON-only fast path that keeps the typed `ingestTx` in hand
and uses a reusable `xdr.EncodingBuffer` scratch buffer for the xdr2json calls
would remove duplicate metadata work plus one layer of per-field/per-event heap
allocation before Rust starts parsing.

## Trigger

1. Issue `getTransactions` with `xdrFormat=json` on pages containing many
   transactions, especially Soroban transactions with large metas and events.
2. Capture allocation profiles around `processTransactionsInLedger`,
   `db.ParseTransaction`, and `xdr.EncodingBuffer.MarshalBinary`.
3. Compare against a prototype that replaces `db.ParseTransaction()` +
   `transactionToJSON(tx)` with a JSON helper that consumes `ingestTx`
   directly and reuses one `EncodingBuffer` per request.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:141-182` — JSON mode already has the typed `ingestTx` and fills much of `txInfo` before calling `db.ParseTransaction()`.
- `cmd/stellar-rpc/internal/db/transaction.go:ParseTransaction:262-301` — repacks the same transaction into `db.Transaction` for the JSON path.
- `cmd/stellar-rpc/internal/db/transaction.go:parseEvents:304-340` — allocates event XDR slices solely so later code can hand them to xdr2json.
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/xdr/main.go:EncodingBuffer.UnsafeMarshalBinary/MarshalBinary:188-227` — existing reusable scratch-buffer API already avoids exactly this class of per-call allocation on the XDR path.

## Evidence

The XDR branch in `processTransactionsInLedger()` already uses one request-scoped
`xdr.EncodingBuffer` and converts directly from typed values, while the JSON
branch detours through `db.Transaction` and allocates new byte slices for fields
that are not stored or reused anywhere after the immediate xdr2json calls. The
duplication is especially visible because `txInfo` is partially populated from
`ingestTx` before `ParseTransaction()` recomputes overlapping state.

## Anti-Evidence

The fast path still needs to serialize typed Go XDR objects back to bytes because
xdr2json only consumes XDR bytes today, so it cannot remove the actual XDR encode
step entirely. The improvement depends on allocation pressure from the transient
transport object being large enough to matter alongside Rust-side XDR parsing and
JSON serialization.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated in this exact form, though substantially overlaps with fail/009 (LCM reparsed across Go and Rust) and fail/005 (core fields bypass batching)
**Failed At**: reviewer

### Trace Summary

Traced the complete JSON path from `processTransactionsInLedger` (get_transactions.go:152-162) through `db.ParseTransaction` (transaction.go:262-301) into `batchConvertTransactionsToJSON` (json.go:105-211). Confirmed that `ParseTransaction` calls `MarshalBinary()` on `ingestTx.Result.Result`, `ingestTx.UnsafeMeta`, `ingestTx.Envelope`, and all events (via `parseEvents`), allocating fresh `[]byte` slices that are held in `pendingTxJSON.tx` until page-level batch conversion. Also read `EncodingBuffer.UnsafeMarshalBinary` (go-stellar-sdk xdr/main.go:188-194) and confirmed it returns a view into the internal `bytes.Buffer` that is overwritten by subsequent marshal calls. The hypothesis's proposed mechanism is fundamentally incompatible with the existing page-level batching architecture.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:142-162` — `txInfo` is partially populated from `ingestTx` (hash, applicationOrder, feeBump, ledger), then JSON branch calls `db.ParseTransaction` and stores `pendingTxJSON{tx, txnIdx}` for deferred batch conversion
- `cmd/stellar-rpc/internal/db/transaction.go:262-301` — `ParseTransaction` re-derives metadata (feeBump, applicationOrder, successful, ledger, hash — duplicating lines 142-150 of get_transactions.go) then calls `MarshalBinary()` for Result, Meta, Envelope, producing 3 freshly allocated `[]byte` slices
- `cmd/stellar-rpc/internal/db/transaction.go:305-341` — `parseEvents` calls `MarshalBinary()` for every DiagnosticEvent, TransactionEvent, and ContractEvent, producing N additional `[]byte` allocations
- `cmd/stellar-rpc/internal/methods/json.go:105-211` — `batchConvertTransactionsToJSON` collects ALL pending byte slices into flat arrays and makes 6 `ConvertBytesSlice` calls for the entire page — all byte slices must be alive simultaneously
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:65-133` — `ConvertBytesSlice` pins Go byte slices with `runtime.Pinner` and passes them directly to Rust via single CGo call — byte slices must be valid Go heap objects with stable addresses
- `go-stellar-sdk@v0.4.0/xdr/main.go:188-194` — `UnsafeMarshalBinary` resets the internal `bytes.Buffer` and returns a slice into it; "subsequent calls to marshaling methods will overwrite the returned buffer"
- `go-stellar-sdk@v0.4.0/xdr/main.go:220-228` — `MarshalBinary` calls `UnsafeMarshalBinary` then allocates a new `[]byte` and copies — functionally equivalent to standalone `MarshalBinary` on XDR types

### Why It Failed

The hypothesis proposes using `EncodingBuffer.UnsafeMarshalBinary` to avoid per-field heap allocations, but this is fundamentally incompatible with the existing page-level batch optimization (`batchConvertTransactionsToJSON`):

1. **Buffer overwrite conflict**: `UnsafeMarshalBinary` returns a view into the internal `bytes.Buffer`. Each subsequent call overwrites the previous result (documented: "Subsequent calls to marshaling methods will overwrite the returned buffer"). Page-level batching requires ALL byte slices (3 core fields × N transactions + all events) to be alive simultaneously when `ConvertBytesSlice` pins and sends them to Rust. You cannot use a reusable buffer and batch.

2. **Per-tx conversion trades batching for allocation savings**: The only way to use `UnsafeMarshalBinary` is to convert each field immediately (per-transaction) instead of batching. This adds ~600 CGo crossings for a 200-tx page (~0.5-1μs each = 300-600μs overhead) while saving only the `MarshalBinary` output copy (~10μs for a 100KB Meta, ~0.5μs for Envelope, ~0.1μs for Result — totaling ~10.6μs per tx × 200 = ~2.1ms). Net savings: ~1.5-1.8ms on a 50-200ms request = 0.75-3.6%.

3. **Duplicate metadata work is negligible**: The re-derived fields (feeBump, applicationOrder, successful, ledger, hash) involve a boolean check, integer conversion, string formatting, and field access — totaling ~1-5μs per transaction, or ~0.2-1ms for a 200-tx page. This is 0.1-2% of request time.

4. **Prior investigations bound the savings**: Fail/009 quantified the complete `MarshalBinary` elimination at "~30-300μs/tx... roughly 1-6% of total per-ledger processing time" and rejected it due to disproportionate implementation complexity. Fail/005 quantified the batching benefit at ~0.3-0.6ms for 200 transactions. Fail/006 demonstrated that even larger per-conversion savings (eliminating two full input copies) achieved only ~2.8% at p50, which fell within run-to-run variance. This hypothesis targets a SMALLER savings than fail/006 (output copy only, not input copies) while LOSING the batching benefit.

5. **`EncodingBuffer.MarshalBinary` is not meaningfully better than standalone `MarshalBinary`**: Both allocate an output `[]byte` of the same size. The only difference is encoder buffer reuse, saving one internal buffer allocation (~64 bytes) per call — negligible.

### Lesson Learned

When proposing to replace a batched architecture with a per-item approach using buffer reuse, verify that the buffer's lifecycle constraints are compatible with the batch API's requirements. `UnsafeMarshalBinary`'s documentation explicitly states the buffer is overwritten on subsequent calls, which is incompatible with any design that needs multiple serialization results alive simultaneously. The XDR format branch can use `EncodingBuffer` because it converts each field to a base64 string (an independent allocation) immediately; the JSON branch cannot because it defers conversion to a page-level batch pass. This structural difference means optimizations from the XDR path cannot be naively transplanted to the JSON path.
