# H004: JSON getTransactions Fans Out Into Many Small CGo XDR-to-JSON Calls Per Transaction

**Date**: 2026-04-06
**Subsystem**: methods
**Severity**: High
**Impact**: latency / CPU / FFI overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

The JSON form of `getTransactions` should minimize CGo boundary crossings by converting a transaction payload in coarse batches, ideally once per transaction or once per ledger. A 50-200 transaction page should not require separate Rust FFI calls for every result blob, envelope, meta blob, diagnostic event, transaction event, and contract event.

## Mechanism

For each transaction, `processTransactionsInLedger` calls `transactionToJSON` (three `ConvertBytes` calls), `jsonifySlice` for diagnostic events, and `BuildEventsJSONFromTransaction`, which makes additional `ConvertBytes` calls for every transaction and contract event. `xdr2json.ConvertBytes` allocates C memory with `C.CBytes`, allocates a type-name string with `C.CString`, crosses into `xdr_to_json`, and copies the JSON result back on every call. On event-heavy JSON pages this turns one RPC into hundreds or thousands of tiny CGo conversions, which should be materially slower than a batched serializer in the shared `xdr2json` crate.

## Trigger

1. Issue `getTransactions` with `format=json` against ledgers containing many Soroban transactions with contract and diagnostic events.
2. Profile CGo call counts, CPU time, and allocation volume for a 50-transaction and 200-transaction page.
3. Compare against a prototype that batches the per-transaction JSON conversion across the FFI boundary.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:162-191` — JSON path performs multiple conversions per returned transaction.
- `cmd/stellar-rpc/internal/methods/json.go:12-36` — `transactionToJSON` does three separate `ConvertBytes` calls.
- `cmd/stellar-rpc/internal/methods/json.go:56-80` — `jsonifySlice` and `jsonifySliceOfSlices` convert each event individually.
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:36-79` — each conversion allocates C buffers and crosses the FFI boundary.

## Evidence

The loaded optimization guidance explicitly calls out serialization overhead and FFI data-copy costs for `getTransactions`, and this code matches that pattern exactly. The JSON path is a fan-out tree of tiny conversions rather than a coarse-grained serialization step.

## Anti-Evidence

This does not affect XDR responses, and the gain will be smaller for transactions with no events. A batched FFI path would require coordinated changes in the shared `xdr2json` implementation, so the fix is more invasive than a local Go-only optimization.

---

## Review

**Verdict**: VIABLE
**Severity**: Medium
**Date**: 2026-04-06
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the complete JSON conversion path from `processTransactionsInLedger` (get_transactions.go:162-191) through `transactionToJSON` (json.go:12-36), `jsonifySlice` (json.go:56-67), and `BuildEventsJSONFromTransaction` (get_transaction.go:143-156), all converging on `xdr2json.ConvertBytes` → `convertAnyBytes` (conversion.go:61-80). Each call crosses the CGo boundary with 2 C allocations (`C.CBytes` for XDR data, `C.CString` for the type name), invokes the Rust `xdr_to_json` function which itself copies the input buffer (`from_c_xdr` → `.to_vec()`), re-parses the type name string (`from_c_string` → `TypeVariant::from_str`), wraps in `panic::catch_unwind`, and allocates C strings for the output. For a 200-transaction page with moderate events (~12 calls/tx), this yields ~2400 CGo crossings; for event-heavy pages (~48 calls/tx), ~9600 crossings.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:162-191` — JSON switch arm: calls `transactionToJSON` (3 FFI calls), `jsonifySlice` for diagnostic events (N calls), `BuildEventsJSONFromTransaction` (M+P calls) per transaction
- `cmd/stellar-rpc/internal/methods/json.go:12-36` — `transactionToJSON`: 3 separate `ConvertBytes` calls for Result, Envelope, Meta
- `cmd/stellar-rpc/internal/methods/json.go:56-67` — `jsonifySlice`: loops over `values` calling `ConvertBytes` per element
- `cmd/stellar-rpc/internal/methods/json.go:71-81` — `jsonifySliceOfSlices`: nested loop calling `jsonifySlice` per inner slice
- `cmd/stellar-rpc/internal/methods/get_transaction.go:143-156` — `BuildEventsJSONFromTransaction`: calls `jsonifySliceOfSlices` for ContractEvents + `jsonifySlice` for TransactionEvents
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:36-42` — `ConvertBytes`: uses `reflect.TypeOf(xdr).Name()` to extract type name, then delegates to `convertAnyBytes`
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:61-80` — `convertAnyBytes`: allocates `C.CBytes(field)` (malloc+memcpy), `C.CString(xdrTypeName)` (malloc+memcpy), calls `C.xdr_to_json`, copies back via `C.GoString` ×2, frees 3 C pointers
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:61-91` — `xdr_to_json`: `panic::catch_unwind` wrapper, `from_c_string` (String alloc), `TypeVariant::from_str` (enum string matching), `from_c_xdr` (Vec alloc+copy), XDR deserialize, JSON serialize, 2× `string_to_c` (CString alloc), Box alloc
- `cmd/stellar-rpc/lib/ffi/src/lib.rs:75-78` — `from_c_string`: creates owned String from C pointer (allocation every call)
- `cmd/stellar-rpc/lib/ffi/src/lib.rs:88-91` — `from_c_xdr`: copies entire XDR buffer into new Vec<u8> (allocation every call)

### Findings

The per-call overhead is substantial relative to actual conversion work for small items (events). Each CGo crossing incurs:

**Go side (~900-1500ns fixed overhead per call):**
- `C.CBytes`: malloc + memcpy of XDR data
- `C.CString`: malloc + memcpy of type name string (identical string repeated thousands of times)
- CGo context switch: ~100-200ns
- `C.GoString` ×2: copies JSON result + error back to Go heap
- 3× `C.free`: type name, conversion result, input XDR

**Rust side (~700-1400ns fixed overhead per call):**
- `panic::catch_unwind`: landing pad setup
- `from_c_string(typename)`: allocates new String (repeated identical string)
- `TypeVariant::from_str`: enum string matching against all XDR type variants (repeated for same type)
- `from_c_xdr(xdr)`: allocates Vec<u8> + memcpy of entire input
- `string_to_c` ×2: allocates CStrings for output
- `Box::into_raw(Box::new(...))`: heap allocation for result struct

**Total fixed overhead: ~1.6-2.9µs per call.** For small events where actual XDR deserialization + JSON serialization takes ~2-4µs, the overhead represents 40-60% of per-call time.

**Impact estimate for a 200-transaction event-heavy page (~9600 CGo calls):**
- Fixed overhead: ~9600 × 2.2µs ≈ 21ms
- Actual conversion work: ~9600 × 3µs ≈ 29ms
- Total conversion time: ~50ms
- Overhead fraction of conversion: ~42%
- Batching could save ~15-21ms of the 50ms conversion time

Comparing to total request time (including DB reads of ~20-40ms), the conversion overhead represents ~15-25% of end-to-end latency. A batch API would reduce this to near zero, yielding an estimated 10-20% total latency improvement for JSON format with event-heavy pages.

**Severity downgrade from High to Medium:** The hypothesis claims High (>20% latency reduction), but the improvement is likely 10-20% for event-heavy pages and smaller for event-light pages, placing it in the Medium range. The improvement is real and measurable but workload-dependent.

### PoC Guidance

- **Target code**: 
  - Rust: `cmd/stellar-rpc/lib/xdr2json/src/lib.rs` — add a new `xdr_batch_to_json` FFI function that accepts an array of (typename, xdr_bytes) pairs and returns an array of JSON strings, resolving each `TypeVariant` once per unique type name and wrapping in a single `panic::catch_unwind`
  - C header: `cmd/stellar-rpc/lib/xdr2json.h` — declare the new batch function with appropriate C types
  - Go: `cmd/stellar-rpc/internal/xdr2json/conversion.go` — add `ConvertBatch(items []BatchItem) ([]json.RawMessage, error)` that marshals all items into a single C buffer, makes one CGo call, and unmarshals all results
  - Go callers: `cmd/stellar-rpc/internal/methods/json.go` and `get_transaction.go` — refactor `jsonifySlice` to use `ConvertBatch` instead of per-item `ConvertBytes`
- **Change description**: Replace the N-calls-per-transaction fan-out with a single batch CGo call that groups all conversions by type. The Rust side resolves each `TypeVariant` once, wraps in one `panic::catch_unwind`, and processes all items of the same type sequentially, returning concatenated results.
- **Correctness check**: Existing tests in `cmd/stellar-rpc/internal/methods/` and `cmd/stellar-rpc/internal/xdr2json/conversion_test.go` cover the JSON conversion path. The batch API must produce byte-identical output to the current per-item API.
- **Benchmark focus**: Measure `getTransactions` with `format=json` latency for pages of 50, 100, and 200 transactions with varying event counts (0, 10, 50 events per transaction). Target metric: p50/p99 latency reduction of 10-20% for event-heavy pages. Also measure CGo call count reduction and allocation volume reduction via pprof.
