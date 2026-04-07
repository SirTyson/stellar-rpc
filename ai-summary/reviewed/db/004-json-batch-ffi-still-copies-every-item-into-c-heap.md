# H004: JSON page batching still pays one C allocation and memcpy per XDR field

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Medium
**Impact**: CPU / allocation / CGo overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When `getTransactions` batches JSON conversion across a page, the remaining FFI overhead should be close to one crossing per field type, not one C heap allocation and memcpy per individual XDR buffer. Event-heavy JSON pages should not spend a large fraction of time copying already-materialized Go byte slices into temporary C buffers.

## Mechanism

Even on the batched path, `ConvertBytesSlice()` still loops over every input buffer and calls `CXDR(field)`, which uses `C.CBytes` to allocate and copy that item into C heap memory before the Rust converter runs. For a large JSON page this means hundreds or thousands of `malloc`/`memcpy`/`free` operations survive after batching; a zero-copy descriptor API or a single arena-with-offsets batch format could preserve the low CGo crossing count while removing the per-item C heap churn.

## Trigger

Request `getTransactions` in `json` format over event-heavy Soroban ledgers with a large limit. Profiles should still show substantial time and allocations under `CXDR`, `C.CBytes`, and `FreeGoXDR` even after the page-level batching hypotheses for core fields and events are applied.

## Target Code

- `cmd/stellar-rpc/internal/methods/json.go:12-36` — core field JSON conversion routes through `ConvertBytes`
- `cmd/stellar-rpc/internal/methods/json.go:56-90` — batched event conversion routes through `ConvertBytesSlice`
- `cmd/stellar-rpc/internal/methods/get_transactions.go:159-180` — JSON path invokes these converters for every returned transaction
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:61-126` — `ConvertBytesSlice()` still performs per-item `CXDR()` setup and teardown
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:149-159` — `CXDR()` uses `C.CBytes`, forcing a separate C heap copy per XDR buffer

## Evidence

`transactionToJSON()` and the slice helpers are the only path from `getTransactions` JSON mode into Rust (`cmd/stellar-rpc/internal/methods/json.go:12-36,56-90`). `ConvertBytesSlice()` reduces the number of CGo crossings but still builds a `[]C.xdr_t` by calling `CXDR(field)` for each non-empty element (`cmd/stellar-rpc/internal/xdr2json/conversion.go:74-103`), and `CXDR()` is a thin wrapper over `C.CBytes` (`cmd/stellar-rpc/internal/xdr2json/conversion.go:149-154`). `getTransactions` invokes these helpers for result, envelope, meta, diagnostic events, transaction events, and contract events on every JSON page item (`cmd/stellar-rpc/internal/methods/get_transactions.go:159-180`).

## Anti-Evidence

If Rust-side XDR parsing and JSON serialization dominate the page, copy elimination alone will not remove all of the latency. Any new FFI shape must still surface per-item conversion errors clearly and stay within cgo pointer-safety rules.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the complete `ConvertBytesSlice` code path from the Go caller through `CXDR` → `C.CBytes` (malloc+memcpy) into the Rust `xdr_batch_to_json` function which reads from the C-allocated buffers via `std::slice::from_raw_parts`, then back through `FreeGoXDR` → `C.free`. Confirmed that batching (H002/H003) amortizes CGo crossings and Rust type resolution but leaves every per-item `C.CBytes` allocation and corresponding `C.free` intact. For a 200-transaction event-heavy page, this means ~5000 individual malloc/memcpy/free cycles survive after batching.

### Code Paths Examined

- `cmd/stellar-rpc/internal/xdr2json/conversion.go:ConvertBytesSlice:64-126` — iterates `fields`, calls `CXDR(field)` per non-empty item (line 83), building `[]C.xdr_t`; after the single `xdr_batch_to_json` crossing, calls `FreeGoXDR` per item (lines 100-102)
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:CXDR:150-155` — wraps `C.CBytes(xdr)` which is `C.malloc(len) + C.memmove`; each call is an independent C heap allocation
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:FreeGoXDR:157-159` — `C.free` per item
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:xdr_batch_to_json:198-270` — Rust reads each `item.xdr` via `std::slice::from_raw_parts` (line 222); does NOT copy the data again, so the C-heap copy is the only intermediate copy
- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:150-187` — JSON path produces 3 core field ConvertBytes calls + 3 event batch calls per transaction; after H002/H003 batching, these become ~6 page-level ConvertBytesSlice calls, each with per-item CXDR

### Findings

The inefficiency is real. Per-item `C.CBytes` inside `ConvertBytesSlice` performs:
- `C.malloc`: ~100ns per call (C allocator overhead)
- `memmove`: varies by item size — ~50ns for small events (~500B), ~5µs for TransactionMeta (~50KB)
- `C.free`: ~50ns per call

For a 200-transaction page with ~25 items per transaction (3 core + ~22 events):
- **5000 malloc/free pairs**: 5000 × 150ns ≈ **750µs** allocator overhead
- **memcpy total**: 200 × 5µs (meta) + 400 × 0.1µs (result/envelope) + 4400 × 50ns (events) ≈ **1.26ms**
- **Combined per-item C heap overhead**: ~**2.0ms**

vs. total Rust-side conversion time of 25-100ms+ (dominated by `read_xdr_to_end` + `serde_json::to_string`). The C heap overhead is **2-8%** of total conversion time.

**Two fix approaches exist:**

1. **Arena approach** (simpler): Single `C.malloc` for total XDR bytes, copy all items contiguously using Go's `copy()` (no CGo crossing), compute per-item pointers as base+offset. Saves ~750µs (malloc/free overhead only); memcpy work unchanged. Net: ~3%.

2. **Zero-copy with `runtime.Pinner`** (Go 1.21+; this repo uses Go 1.25): Allocate the `items` array in C memory (`C.malloc(N * sizeof(xdr_t))`), pin each Go byte slice with `runtime.Pinner`, store Go data pointers directly in the C-allocated `xdr_t` structs, pass to Rust. Eliminates both malloc/free and memcpy. However: Pin/Unpin has atomic operation overhead (~75ns/item = ~375µs for 5000 items), and the C-allocated items array needs its own malloc/free. Net savings: ~1.6ms (~6%). Complexity is significantly higher and CGo pointer safety must be carefully maintained (Go pointers stored in C memory, not Go memory, so the "Go memory containing Go pointers" rule is satisfied).

**Severity downgrade**: Medium → **Low**. The per-item C heap overhead is real and survives batching, but it represents <5% of total conversion time for the practical arena fix. The zero-copy approach could reach ~6% but introduces fragile CGo pointer management. The dominant cost is always the Rust XDR parsing and JSON serialization work, which is identical regardless of how memory is passed across the FFI boundary.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/xdr2json/conversion.go` — modify `ConvertBytesSlice` to use an arena allocation strategy
- **Change description**: Replace the per-item `CXDR(field)` loop with: (1) compute `totalSize` = sum of all field lengths, (2) single `C.malloc(totalSize)` for an arena, (3) `copy()` each field into the arena at successive offsets (no CGo crossing for the copy), (4) build `items[i].xdr` as arena base + offset, (5) single `C.free(arena)` after `xdr_batch_to_json` returns. This eliminates N-1 malloc/free pairs while keeping the same memcpy total. The Rust side needs no changes — it receives the same `xdr_t*` array.
- **Correctness check**: Existing tests in `cmd/stellar-rpc/internal/xdr2json/conversion_test.go` cover `ConvertBytesSlice`. The `BenchmarkConvertBytesVsSlice` benchmark can be extended to measure arena vs. per-item allocation. Integration tests for JSON-format `getTransactions` responses should pass unchanged.
- **Benchmark focus**: Measure allocs/op reduction in `BenchmarkConvertBytesVsSlice` with 500-2000 items. Expect ~750µs reduction in malloc/free overhead per page. For a more complete picture, benchmark `processTransactionsInLedger` with 200 Soroban transactions in JSON mode and compare total ns/op.
