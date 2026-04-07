# H003: JSON `getTransactions` Builds a Short-Lived Byte Graph Only to Walk It Once

**Date**: 2026-04-07
**Subsystem**: methods
**Severity**: Low
**Impact**: CPU / allocations / GC
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

For `format=json`, the handler should convert each returned transaction with minimal intermediate heap state. It should not first build a byte-oriented `db.Transaction` object containing result/meta/envelope/event buffers when those buffers are consumed exactly once and discarded immediately after JSON conversion.

## Mechanism

`processTransactionsInLedger` always calls `db.ParseTransaction` before the format switch. `ParseTransaction` eagerly marshals `Result`, `Meta`, `Envelope`, `DiagnosticEvents`, `TransactionEvents`, and `ContractEvents` into nested byte slices stored on `db.Transaction`; the JSON branch then immediately traverses that object again via `transactionToJSON`, `jsonifySlice`, and `BuildEventsJSONFromTransaction`. A JSON-specific builder that marshals each XDR value directly into the `xdr2json` batch helpers and response fields could remove the short-lived `db.Transaction` byte graph and cut allocation/GC pressure on event-heavy JSON pages.

## Trigger

1. Issue `getTransactions` with `format=json` against ledgers containing many diagnostic, transaction, and contract events.
2. Capture allocation profiles and retained heap during a 50-200 transaction page.
3. Compare against a version that bypasses `db.Transaction` for the JSON path and streams each marshaled item directly into JSON conversion/output assembly.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:129-176` — the handler calls `db.ParseTransaction` before selecting the JSON path.
- `cmd/stellar-rpc/internal/db/transaction.go:258-319` — `ParseTransaction` eagerly marshals every returned field and event family into byte slices on `db.Transaction`.
- `cmd/stellar-rpc/internal/methods/json.go:12-90` — the JSON helpers immediately walk those byte slices again for conversion.
- `cmd/stellar-rpc/internal/methods/get_transaction.go:143-155` — event JSON conversion consumes the already-marshaled slices rather than typed XDR values.

## Evidence

The `db.Transaction` type is byte-backed by design (`[]byte`, `[][]byte`, `[][][]byte` fields), and the JSON branch never uses the XDR/base64 helpers that motivated that representation. In the hot path, the object exists only long enough to feed `xdr2json`, which makes it a clear candidate for a format-specific fast path that trades one generic intermediate representation for direct JSON assembly.

## Anti-Evidence

`xdr2json` still ultimately needs XDR bytes, so this optimization removes heap churn more than it removes the underlying marshaling work. The gain is therefore JSON-only and likely smaller than planner/read-path fixes that cut entire ledger fetches or decodes.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated (distinct from 006-json-ffi-call-explosion which targeted CGo call batching, not intermediate struct elimination)
**Failed At**: reviewer

### Trace Summary

Traced `processTransactionsInLedger` (get_transactions.go:129) → `db.ParseTransaction` (transaction.go:238-291) → `MarshalBinary()` on Result, Meta, Envelope, and all events → stores `[]byte` fields on `db.Transaction`. Then the JSON path calls `transactionToJSON` (json.go:12-36) → `xdr2json.ConvertBytes` which takes those same `[]byte` and feeds them through CGo to the Rust `xdr_to_json`. Also checked `xdr2json.ConvertInterface` (conversion.go:51-59), which internally calls `MarshalBinary()` + `convertAnyBytes` — identical work. The proposed bypass cannot eliminate the `MarshalBinary()` step because the xdr2json FFI requires XDR bytes as input.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:129-135` — `db.ParseTransaction(ledger, ingestTx)` called unconditionally before format switch
- `cmd/stellar-rpc/internal/db/transaction.go:238-291` — `ParseTransaction` calls `MarshalBinary()` on Result (line 258), Meta (line 261), Envelope (line 264), diagnostic events (lines 279-285), transaction events (lines 297-303), contract events (lines 308-318)
- `cmd/stellar-rpc/internal/methods/json.go:12-36` — `transactionToJSON` passes `tx.Result`, `tx.Meta`, `tx.Envelope` (the `[]byte` fields) to `xdr2json.ConvertBytes`
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:36-42` — `ConvertBytes` takes `[]byte` input, resolves type name, delegates to `convertAnyBytes`
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:51-59` — `ConvertInterface` calls `xdr.MarshalBinary()` internally then delegates to same `convertAnyBytes` — no savings vs. pre-marshaling
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:64-126` — `ConvertBytesSlice` (batch API) also requires `[]byte` input per item

### Why It Failed

The hypothesis proposes eliminating the `db.Transaction` intermediate byte representation for the JSON path, but the optimization is illusory for three reasons:

1. **`xdr2json` requires XDR bytes as input.** The Rust FFI (`xdr_to_json`, `xdr_batch_to_json`) accepts `xdr_t` structs containing raw XDR byte buffers. There is no typed-XDR-to-JSON path. `ConvertInterface` (the only alternative to `ConvertBytes`) internally calls `MarshalBinary()` first — producing the exact same bytes. Skipping `ParseTransaction` would just move the `MarshalBinary()` calls from `ParseTransaction` into `ConvertInterface`, with zero net savings.

2. **Struct field storage vs. local variables has negligible cost.** Whether a `[]byte` is stored in a `db.Transaction` struct field or a local variable makes no difference to Go's allocator or GC. The heap allocation is for the byte slice backing array, not the pointer/length/capacity header. The `db.Transaction` struct itself is stack-allocatable (it doesn't escape the function).

3. **Empirical evidence from the related H004/006 investigation confirms this path is not the bottleneck.** The closely related hypothesis 006-json-ffi-call-explosion proposed batching all CGo calls (a much larger optimization targeting ~1.6-2.9µs of fixed overhead per FFI crossing × thousands of crossings). It was implemented, benchmarked with `stellar-rpc-blaster`, and showed **no improvement** — in fact it regressed at moderate/high loads. If eliminating thousands of CGo crossings produced no gain, eliminating struct field assignments within the same path cannot produce a measurable improvement.

### Lesson Learned

The JSON conversion path in `getTransactions` has been empirically validated (via 006-json-ffi-call-explosion benchmarking) as non-dominant in end-to-end latency. Future hypotheses targeting allocations or GC in this path should account for the fact that DB reads and ledger meta deserialization dominate request time, making serialization-layer micro-optimizations ineffective.
