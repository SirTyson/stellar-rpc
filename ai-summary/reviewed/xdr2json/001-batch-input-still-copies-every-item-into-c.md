# H001: Batch xdr2json Still Copies Every Item Into a Fresh C Buffer

**Date**: 2026-04-07
**Subsystem**: xdr2json
**Severity**: Low
**Impact**: latency / CPU / allocations / memory bandwidth
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once `getTransactions` is already using the batched `xdr_batch_to_json` path for diagnostic, contract, and transaction events, the batch bridge should hand Rust a view of the caller-owned XDR bytes for the duration of that synchronous FFI call. Event-heavy pages should not still pay one `malloc + memcpy + free` cycle per event before Rust even starts parsing.

## Mechanism

`ConvertBytesSlice` builds its `items []C.xdr_t` array by calling `CXDR(field)` for every non-empty element, and `CXDR` still uses `C.CBytes(field)`. Rust's `xdr_batch_to_json` now borrows each `item.xdr` directly with `slice::from_raw_parts(item.xdr, item.len)` and does not clone or retain the bytes after the call returns, so the remaining Go-side copy in the batch path is pure transport overhead. On `getTransactions` pages with thousands of events, that leaves the batch API paying per-item allocator churn and memory bandwidth even though the boundary crossing itself has already been amortized.

## Trigger

1. Issue `getTransactions` with `format=json` against a 100-200 transaction page with many diagnostic, contract, or transaction events.
2. Profile allocations and copied bytes around `C.CBytes`, `FreeGoXDR`, and `ConvertBytesSlice`.
3. Compare against a prototype that fills `[]C.xdr_t` with `unsafe.Pointer(&field[0])` into Go-owned byte slices and keeps those slices alive until `xdr_batch_to_json` returns.

## Target Code

- `cmd/stellar-rpc/internal/xdr2json/conversion.go:ConvertBytesSlice:64-125` — batch setup still materializes `CXDR` per item and frees each copied buffer afterward.
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:CXDR/FreeGoXDR:149-159` — the batch path still uses `C.CBytes` and `C.free`.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:xdr_batch_to_json:198-239` — Rust only borrows each `item.xdr` during the synchronous loop.
- `cmd/stellar-rpc/internal/methods/json.go:jsonifySlice/jsonifySliceOfSlices:56-90` — `getTransactions` event JSON conversion funnels through the batched path.

## Evidence

The batch API already collapsed event conversion down to one FFI call per homogeneous group, so the remaining fixed cost in that path is the per-item transport work done before `xdr_batch_to_json` starts. The Rust side no longer clones the input, and the batch result is consumed synchronously, which makes the current per-item `C.CBytes` allocation look like the main unoptimized input-copy cost left on the hot event path.

## Anti-Evidence

This relies on staying within cgo's pointer-lifetime rules: the batch call must never retain references after return, and the Go slices must remain pinned by reachability for the full call. Small event payloads will see less benefit than large metas or large diagnostic payloads, so the gain is likely measurable but not dominant.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full batch input path from `jsonifySlice` / `jsonifySliceOfSlices` (json.go:56-90) through `ConvertBytesSlice` (conversion.go:64-125) into `xdr_batch_to_json` (lib.rs:198-242). Confirmed that each non-empty field gets `CXDR(field)` → `C.CBytes` (malloc + memcpy, line 83/152), the Rust side only borrows via `slice::from_raw_parts(item.xdr, item.len)` (line 222) without cloning, and the Go side frees each C buffer post-call (lines 100-102). The Rust `xdr_batch_to_json` is synchronous and does not retain any input pointers beyond the call boundary, so the per-item C.CBytes copy is pure transport overhead. However, the proposed fix mechanism (using `unsafe.Pointer(&field[0])` directly) would violate cgo pointer-passing rules — a corrected approach using `runtime.Pinner` (Go 1.21+, available in the project's Go 1.25) is viable.

### Code Paths Examined

- `cmd/stellar-rpc/internal/xdr2json/conversion.go:83` — `CXDR(field)` called per non-empty element; triggers `C.CBytes` which is malloc(len) + memmove(len)
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:150-154` — `CXDR` uses `C.CBytes(xdr)` → C.malloc + C.memmove; returns `xdr_t{pointer, len}`
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:100-102` — `FreeGoXDR(item)` → `C.free` called per item after the FFI returns
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:218` — Rust creates `items_slice` from the C array pointer; no copy
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:222` — `slice::from_raw_parts(item.xdr, item.len)` borrows directly; no clone
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:223` — `xdr::Limited::new(xdr_slice, ...)` wraps the borrowed slice for deserialization
- `cmd/stellar-rpc/internal/methods/json.go:56-57` — `jsonifySlice` delegates to `ConvertBytesSlice`
- `cmd/stellar-rpc/internal/methods/json.go:62-90` — `jsonifySliceOfSlices` flattens all inner slices into one batch call
- `cmd/stellar-rpc/internal/methods/get_transaction.go:98,147,151` — getTransaction uses batch path for DiagnosticEvent, ContractEvent, TransactionEvent
- `cmd/stellar-rpc/internal/methods/get_transactions.go:167,180` — getTransactions similarly batches events
- `cmd/stellar-rpc/lib/shared.h:1-4` — `xdr_t` struct: `unsigned char *xdr` + `size_t len`

### Findings

**The inefficiency is real.** Each non-empty event in a batch goes through `C.CBytes` (malloc + memcpy) and later `C.free`, despite the Rust side only borrowing the data synchronously. For a 200-transaction page with ~4000 events, this is ~4000 malloc/free cycles plus ~4000 memcpy operations.

**Quantified impact (estimated):**
- Per-item allocator overhead (malloc + free): ~100-200ns per item
- Per-item memcpy: ~10-50ns for small events (~300 bytes), ~10μs for large events (~100KB)
- For 4000 small events: ~0.4-1.0ms allocator churn + ~0.04-0.2ms memcpy = ~0.5-1.2ms total
- For 100 large events (100KB): ~0.01-0.02ms allocator + ~1ms memcpy = ~1ms total
- Total request time for a 200-tx JSON page: ~50-200ms
- Estimated savings: ~0.5-2% for typical workloads; up to ~2-3% for large-payload events

**cgo pointer-passing rule complication.** The hypothesis proposes filling `[]C.xdr_t` with `unsafe.Pointer(&field[0])`. This would store Go pointers inside a Go-allocated slice, then pass `&items[0]` to C — violating Go's cgo rule: "Go memory passed to C must not contain Go pointers." The cgo checker (enabled by default) would panic at runtime.

**Correct implementation via `runtime.Pinner` (Go 1.21+).** Pinning each `field[0]` causes the cgo checker to treat those Go pointers as C-equivalent. This eliminates all per-item malloc/memcpy/free at the cost of Pin/Unpin overhead (~50-100ns per item). Net savings: ~50-150ns per small item, dominated by memcpy savings for large items. The project uses Go 1.25, so `runtime.Pinner` is available.

**Alternative: single-buffer consolidation.** Allocate one C buffer (`C.malloc(totalBytes)`), copy all fields contiguously, set each `items[i].xdr` to the appropriate offset within the single buffer. This preserves cgo safety without `runtime.Pinner` and eliminates N-1 malloc/free cycles, though total memcpy bytes remain the same.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/xdr2json/conversion.go:ConvertBytesSlice` (lines 64-125), specifically the loop at lines 78-85 and cleanup at lines 100-102
- **Change description**: Replace per-item `CXDR(field)` / `FreeGoXDR(item)` with `runtime.Pinner`-based zero-copy approach:
  ```go
  var pinner runtime.Pinner
  defer pinner.Unpin()
  for i, field := range fields {
      if len(field) == 0 {
          result[i] = json.RawMessage("")
          continue
      }
      pinner.Pin(&field[0])
      items = append(items, C.xdr_t{
          xdr: (*C.uchar)(unsafe.Pointer(&field[0])),
          len: C.size_t(len(field)),
      })
      indices = append(indices, i)
  }
  // Remove the FreeGoXDR loop (lines 100-102) — no C buffers to free
  ```
- **Correctness check**: Existing tests in `conversion_test.go` cover `ConvertBytesSlice`. The Rust-side test `borrowed_slice_avoids_extra_clone_for_large_diagnostic_event` (lib.rs:372-395) verifies borrow semantics. Run `make go-test` and `cargo test` in `cmd/stellar-rpc/lib/xdr2json/`.
- **Benchmark focus**: Measure allocation count and total allocated bytes in `ConvertBytesSlice` for a batch of 1000+ events. Use Go's `testing.B` with `b.ReportAllocs()`. Expected: zero C-side allocations for input buffers (down from N), ~N×avg_event_size fewer bytes allocated. Latency improvement: ~0.5-1ms on a 200-tx page with small events; larger gains for big diagnostic events (>10KB payloads).
