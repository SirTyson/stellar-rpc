# H001: xdr2json Copies Every getTransactions XDR Payload Twice Before Rust Starts Parsing

**Date**: 2026-04-07
**Subsystem**: xdr2json
**Severity**: Medium
**Impact**: latency / CPU / allocations / memory bandwidth
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When `getTransactions` already has a serialized XDR blob for a transaction field or event, the `xdr2json` bridge should let Rust deserialize directly from that caller-owned buffer for the duration of the FFI call. A JSON page with 50-200 transactions should not memcpy every `TransactionResult`, `TransactionEnvelope`, `TransactionMeta`, diagnostic event, transaction event, and contract event once in Go and then again in Rust before deserialization begins.

## Mechanism

`xdr2json.ConvertBytes` first calls `CXDR`, which allocates a new C buffer with `C.CBytes(field)`. Rust then immediately calls `from_c_xdr(xdr)` and performs `slice::from_raw_parts(...).to_vec()`, allocating and copying the same payload again before `read_xdr_to_end` consumes it. On the `getTransactions` JSON path this happens for every per-transaction and per-event conversion, so large `TransactionMeta` blobs and event-heavy pages pay two avoidable input copies per item before any JSON serialization work starts.

## Trigger

1. Issue `getTransactions` with `format=json` against ledgers containing large Soroban transaction metas and many events.
2. Capture allocation and memcpy profiles while focusing on `C.CBytes`, `from_c_xdr`, and total bytes copied per request.
3. Compare against a prototype that passes a borrowed `[]byte` pointer through `xdr_t` for the synchronous call and replaces `from_c_xdr(...).to_vec()` with a borrowed slice reader in Rust.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:162-185` — JSON responses invoke `transactionToJSON`, `jsonifySlice`, and `BuildEventsJSONFromTransaction` for every returned item.
- `cmd/stellar-rpc/internal/methods/json.go:21-31,56-61` — hot `getTransactions` JSON helpers repeatedly call `xdr2json.ConvertBytes`.
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:61-69,82-91` — Go allocates a fresh C buffer for every input payload.
- `cmd/stellar-rpc/lib/ffi/src/lib.rs:88-90` — `from_c_xdr` copies the C buffer into a new `Vec<u8>`.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:72-75` — the copied `Vec<u8>` is wrapped in `Limited` and immediately parsed.

## Evidence

The current bridge makes two full input copies per conversion even though `xdr_to_json` is synchronous and only needs read-only access to the bytes during the call. `getTransactions` amplifies that cost because its JSON path fans out into repeated `ConvertBytes` calls for the same transaction, especially around `TransactionMeta` and event payloads.

## Anti-Evidence

The optimization depends on obeying cgo pointer-lifetime rules: Rust must not retain the borrowed pointer after the call returns. Small event payloads will benefit less than large metas, so the gain is workload-dependent rather than universal.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full `getTransactions` JSON path from `get_transactions.go:162` through `transactionToJSON`, `jsonifySlice`, and `BuildEventsJSONFromTransaction`, each of which calls `xdr2json.ConvertBytes`. Every call flows through `convertAnyBytes` (conversion.go:61) which allocates a C-side copy via `C.CBytes` (copy #1), then crosses FFI into `xdr_to_json` (lib/xdr2json/src/lib.rs:65) where `from_c_xdr` (lib/ffi/src/lib.rs:88) does `slice::from_raw_parts().to_vec()` (copy #2). Both copies are confirmed unnecessary for this synchronous call — the data is immediately consumed by `Limited::new(...).read_xdr_to_end()` and never retained.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:162-191` — Per-transaction JSON path: 3 `ConvertBytes` calls (Result, Envelope, Meta) + N calls for DiagnosticEvents + M calls for ContractEvents + K calls for TransactionEvents
- `cmd/stellar-rpc/internal/methods/json.go:12-37,56-68` — `transactionToJSON` and `jsonifySlice` delegate every item to `ConvertBytes`
- `cmd/stellar-rpc/internal/methods/get_transaction.go:143-156` — `BuildEventsJSONFromTransaction` adds `jsonifySliceOfSlices` for ContractEvents and `jsonifySlice` for TransactionEvents
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:61-69` — `convertAnyBytes`: `CXDR(field)` calls `C.CBytes(xdr)` which does `malloc + memcpy` (copy #1)
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:82-88` — `CXDR` and `FreeGoXDR`: allocation and deallocation of the C-side buffer
- `cmd/stellar-rpc/lib/ffi/src/lib.rs:88-91` — `from_c_xdr`: `slice::from_raw_parts(xdr.xdr, xdr.len).to_vec()` allocates a Rust `Vec<u8>` and copies (copy #2)
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:72-78` — The `Vec<u8>` is immediately consumed via `.as_slice()` → `Limited::new` → `read_xdr_to_end`; the owned Vec is never needed

### Findings

**Both copies are eliminable.** The call is synchronous and `xdr_to_json` never retains the input bytes past its return:

1. **Copy #1 (Go → C via `C.CBytes`)**: Under Go 1.25 cgo rules, Go memory that contains no Go pointers may be passed directly to C for synchronous calls. A `[]byte` backing array is pure bytes with no Go pointers. The Go side can construct `xdr_t{xdr: (*C.uchar)(unsafe.Pointer(&field[0])), len: C.size_t(len(field))}` and skip `C.CBytes` entirely, eliminating one `malloc + memcpy + free` per call.

2. **Copy #2 (C buffer → Rust `Vec<u8>` via `from_c_xdr`)**: The `Vec<u8>` is only used via `.as_slice()` passed to `Limited::new`. The Rust side can create `&[u8]` directly from the raw pointer via `slice::from_raw_parts(xdr.xdr, xdr.len)` and pass that to `Limited::new`, eliminating one Rust allocation + memcpy per call.

3. **No existing mitigations**: No `sync.Pool`, caching, or batching exists in the xdr2json or json.go paths. Every `ConvertBytes` call independently allocates, copies twice, and frees.

4. **Correctness is preserved**: `read_xdr_to_end` consumes the byte slice by reading and produces a parsed `xdr::Type` — it does not retain a reference to the input bytes. The `catch_unwind` closure runs synchronously on the same thread, so the borrowed pointer remains valid throughout. The `CXDR` struct is `Copy` and captures by value in the `move` closure, preserving the pointer.

5. **Call frequency per `getTransactions` response**: For a page of T transactions with E events each: `3T + sum(diagnostic_events) + sum(contract_events) + sum(transaction_events)` conversions. A page of 50 Soroban transactions with 20 events each yields ~1000+ double-copy operations. With 200KB TransactionMetas, total unnecessary copying exceeds 20MB per response.

6. **Impact estimate**: Per-conversion overhead from the two copies includes ~50-500ns allocator cost (malloc/free on each side) plus memcpy time proportional to payload size. For large metas (~50-200KB), the copy overhead is 5-15% of the per-conversion cost. Across a full page response, aggregate savings are estimated at 0.5-4ms out of 10-50ms total — measurable but typically under 5%.

**Severity downgraded to Low**: While the inefficiency is real and the fix is correct, memcpy and allocation overhead is a small fraction of total per-call cost (dominated by XDR deserialization and JSON serialization). The improvement is measurable but likely <5% for typical workloads. Could approach Medium (5-10%) for extreme Soroban-heavy pages with many large metas and events.

### PoC Guidance

- **Target code (Go side)**: `cmd/stellar-rpc/internal/xdr2json/conversion.go` — modify `convertAnyBytes` to construct `C.xdr_t` from `unsafe.Pointer(&field[0])` instead of `C.CBytes(field)`, and remove the `FreeGoXDR` defer. Guard against empty slices (already handled by early return in `ConvertBytes`).
- **Target code (Rust side)**: `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:72-73` — replace `from_c_xdr(xdr)` with `slice::from_raw_parts(xdr.xdr, xdr.len)` and pass the slice directly to `Limited::new`.
- **Correctness check**: Existing xdr2json unit tests and integration tests for `getTransactions` with `format=json` should pass unchanged. Run `go test ./cmd/stellar-rpc/internal/xdr2json/...` and any integration tests exercising the JSON format path.
- **Benchmark focus**: Measure per-call allocation count (should drop by 2 per conversion) and total bytes allocated. For latency, benchmark `getTransactions` with large Soroban metas (100KB+ TransactionMeta, 20+ events per tx, 50+ txs per page). Expect <5% latency improvement on typical loads, possibly 5-10% on extreme loads.
