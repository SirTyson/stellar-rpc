# H002: xdr2json Copies JSON Output Through a C String and a Go String Before Returning RawMessage

**Date**: 2026-04-07
**Subsystem**: xdr2json
**Severity**: Medium
**Impact**: latency / CPU / allocations / memory bandwidth
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once Rust has serialized an XDR object to JSON, the result should cross the FFI boundary into Go with at most one payload copy into the final `json.RawMessage`. Large `getTransactions` JSON pages should not re-copy every serialized result, envelope, meta blob, and event body through multiple transient string representations on the success path.

## Mechanism

`xdr_to_json` builds a Rust `String` with `serde_json::to_string`, then `string_to_c` allocates a second null-terminated copy for the FFI result. On the Go side, `C.GoString(result.json)` copies that buffer again into a Go `string`, and `json.RawMessage(jsonStr)` copies the bytes a fourth time into the final slice-backed response field. Because `TransactionMeta` JSON and event JSON are often larger than their XDR inputs, this extra output copying can consume measurable CPU and allocation budget even if the call count stays the same.

## Trigger

1. Issue `getTransactions` with `format=json` against ledgers with large `resultMeta`, `diagnosticEvents`, and contract-event payloads.
2. Capture allocation profiles on the Rust-to-Go transfer path, especially around `serde_json::to_string`, `string_to_c`, `C.GoString`, and `json.RawMessage(...)`.
3. Compare against a prototype that returns `(ptr,len)` JSON bytes from Rust and materializes them with a single `C.GoBytes`/owned-buffer transfer into Go.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:164-185` — `getTransactions` stores many separate JSON fragments returned by `xdr2json`.
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:69-79` — Go copies the JSON result into a `string` and then into `json.RawMessage`.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:80-89` — Rust serializes to `String` and then allocates C strings for both `json` and `error`.
- `cmd/stellar-rpc/lib/ffi/src/lib.rs:29-46` — `safe_cstring`/`string_to_c` scan and copy the full JSON payload into a new `CString`.
- `cmd/stellar-rpc/lib/xdr2json.h:3-11` — the FFI result shape exposes only null-terminated strings, forcing the string bridge.

## Evidence

The current API returns JSON as a C string even though Go ultimately wants a byte slice (`json.RawMessage`). That design forces extra success-path allocations and copies for every converted fragment, and `getTransactions` exercises the path repeatedly on the largest JSON-producing types in the codebase.

## Anti-Evidence

At least one copy into Go-owned memory is still required before the response escapes the cgo call, so this is not a true zero-copy output path. Small JSON fragments will see a smaller gain than large metas or event batches.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full JSON output path from Rust serialization through the FFI boundary into the Go caller. Confirmed that the success path performs two unnecessary data traversals beyond the required single FFI copy: (1) a NUL-scan + possible reallocation in `safe_cstring` on the Rust side, and (2) a full-payload copy on the Go side when converting the intermediate `string` to `[]byte` for `json.RawMessage`. The `getTransactions` handler invokes this path 3+ times per transaction (Result, Envelope, Meta) plus once per diagnostic event, so the waste scales with page size.

### Code Paths Examined

- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:81` — `serde_json::to_string(&t)` creates the initial Rust `String` (unavoidable serialization output)
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:88` — `string_to_c(result.json)` calls `safe_cstring` which scans for interior NULs (O(n)) then `.into_raw()` leaks the CString; the `String→CString` conversion is a move but the NUL scan is redundant since JSON never contains NUL bytes
- `cmd/stellar-rpc/lib/ffi/src/lib.rs:29-36` — `safe_cstring` performs `CString::new(str)` which takes ownership of the String's Vec<u8> but scans the entire buffer for NUL bytes
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:72` — `C.GoString(result.json)` performs strlen (O(n)) + malloc + memcpy to create a Go string
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:79` — `json.RawMessage(jsonStr)` converts string→[]byte, which in Go always copies the backing array (strings are immutable, slices are mutable)
- `cmd/stellar-rpc/internal/methods/json.go:21-31` — `transactionToJSON` calls `ConvertBytes` three times per transaction (Result, Envelope, Meta)
- `cmd/stellar-rpc/internal/methods/json.go:56-67` — `jsonifySlice` calls `ConvertBytes` once per diagnostic event
- `cmd/stellar-rpc/internal/methods/get_transactions.go:162-183` — `getTransactions` loops over all transactions on the page, calling the above for each

### Findings

**The inefficiency is real.** The current path performs these operations on the JSON output per call:

| Step | Operation | Cost |
|------|-----------|------|
| Rust: `serde_json::to_string` | Serialization | Required — creates the data |
| Rust: `safe_cstring` NUL scan | O(n) scan of JSON bytes | Unnecessary — valid JSON never contains NUL |
| Rust: `CString::new` move | Usually O(1), sometimes realloc+copy for NUL terminator | Minor waste |
| Go: `C.GoString` strlen | O(n) scan to find length | Unnecessary if length is passed alongside pointer |
| Go: `C.GoString` memcpy | O(n) copy into Go string | Required — must copy into GC-managed memory |
| Go: `string→[]byte` | O(n) copy | Unnecessary — if Go received bytes directly, this vanishes |

Net waste per call: **one full-payload copy + two O(n) scans** (NUL scan in Rust, strlen in Go).

**Aggregate impact for `getTransactions`:** With ~200 transactions per page, each producing Result (~1KB), Envelope (~5KB), Meta (~50KB JSON), plus diagnostic events, the per-page waste is roughly 800+ calls × ~50KB average = ~40MB of unnecessary memory bandwidth. This is measurable in profiles but likely <5% of total request latency since XDR deserialization and JSON serialization dominate.

**Severity downgrade rationale:** The hypothesis claims Medium (5–20%). The actual savings — eliminating one memcpy and two linear scans per call — are real but modest relative to the dominant costs (XDR parsing via `read_xdr_to_end`, JSON serialization via `serde_json::to_string`, and the unavoidable FFI copy). Low (<5% but measurable) is the appropriate severity.

**No correctness concerns with the proposed fix.** `json.RawMessage` is `type RawMessage []byte`, so receiving bytes directly via `C.GoBytes` is type-compatible. No callers depend on the intermediate string representation.

### PoC Guidance

- **Target code**:
  - `cmd/stellar-rpc/lib/xdr2json/src/lib.rs`: Change `ConversionResult` to hold `(*mut u8, usize)` for JSON instead of `*mut c_char`. Convert the Rust String to bytes via `into_bytes()`, box the slice, and leak with `Box::into_raw`. Update `free_conversion_result` to reconstruct and drop the boxed slice.
  - `cmd/stellar-rpc/lib/xdr2json.h`: Update `conversion_result_t` to use `const uint8_t* json_ptr; size_t json_len;` instead of `const char* const json;`.
  - `cmd/stellar-rpc/internal/xdr2json/conversion.go`: Replace `C.GoString(result.json)` + `json.RawMessage(jsonStr)` with `C.GoBytes(unsafe.Pointer(result.json_ptr), C.int(result.json_len))` to produce `[]byte` directly.
  - `cmd/stellar-rpc/lib/ffi/src/lib.rs`: No changes needed — `string_to_c` is simply no longer called for the JSON field.
- **Change description**: Replace the null-terminated C string bridge for JSON output with a `(ptr, len)` byte buffer. This eliminates the NUL scan in `safe_cstring`, the `strlen` in `C.GoString`, and the `string→[]byte` copy in Go, reducing the output path from 3 copies + 2 scans to 1 copy.
- **Correctness check**: Existing Go tests in `cmd/stellar-rpc/internal/methods/get_transactions_test.go`, `get_events_test.go`, and `get_transaction_test.go` exercise `ConvertBytes`/`ConvertInterface` through the JSON format path. The Rust-side `xdr_to_json` is also tested via integration tests. All should pass unchanged since the output bytes are identical.
- **Benchmark focus**: Measure per-page allocation count and bytes allocated for `getTransactions` with `format=json` on event-heavy ledgers. Expect ~40MB reduction in total allocations per full page. Latency improvement likely <5% but should be visible in allocation profiles.
