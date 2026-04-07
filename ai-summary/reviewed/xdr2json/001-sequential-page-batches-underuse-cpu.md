# H001: `batchConvertTransactionsToJSON` Still Runs Six Independent xdr2json Batches in Series

**Date**: 2026-04-07
**Subsystem**: xdr2json
**Severity**: High
**Impact**: latency / CPU / multicore utilization
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once a `getTransactions` JSON page has already collected the page-wide `[][]byte`
inputs for results, envelopes, metas, diagnostic events, transaction events, and
contract events, the conversion stage should use available CPU cores. The handler
should not wait for six CPU-bound Rust batch conversions to finish one after the
other when their inputs and outputs are independent.

## Mechanism

`batchConvertTransactionsToJSON()` builds all six input batches first, then issues
six synchronous `ConvertBytesSlice()` calls in strict sequence. Each call crosses
into Rust and runs a read-only parse + `serde_json` loop with no dependency on the
other five groups, so the current design turns a naturally parallel workload into
sum-of-batches wall time. On Soroban-heavy pages where metas and event groups each
take milliseconds, launching these six batches concurrently with ordered result
assignment should reduce latency toward the slowest batch instead of the sum.

## Trigger

1. Issue `getTransactions` with `format=json` and a 100-200 transaction page.
2. Use ledgers with large `TransactionMeta` blobs plus many diagnostic,
   transaction, and contract events.
3. Compare current wall time against a prototype that dispatches the six
   `ConvertBytesSlice()` calls concurrently and joins before assignment.

## Target Code

- `cmd/stellar-rpc/internal/methods/json.go:105-210` — page-level conversion
  constructs all inputs up front, then calls `ConvertBytesSlice()` six times at
  lines 122, 126, 130, 144, 158, and 179.
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:65-133` — each batch call is
  synchronous and only reads pinned input slices while producing fresh output
  buffers.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:208-289` — `xdr_batch_to_json()`
  contains no shared mutable state between batches.

## Evidence

The Go side already paid the cost to flatten and materialize each homogeneous
batch before the first FFI call starts, so there is no remaining data dependency
between result/envelope/meta/event conversion groups. The Rust entrypoint only
borrows the input slices for the duration of the call and allocates owned output
buffers, which makes concurrent calls a plausible fit for multicore hosts.

## Anti-Evidence

Small pages and single-core deployments will see little benefit, and one dominant
batch can cap the gain. Running all six groups in parallel also increases peak
memory pressure and may need a bounded fan-out to avoid oversubscribing CPUs when
many requests are already in flight.

---

## Review

**Verdict**: VIABLE
**Severity**: Medium
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full path from `getTransactions` handler (get_transactions.go:370-371) through `batchConvertTransactionsToJSON` (json.go:105-210) into six sequential `ConvertBytesSlice` calls (conversion.go:65-132), each crossing CGo into `xdr_batch_to_json` (lib.rs:208-289). Confirmed all six calls operate on disjoint input slices and produce independent output buffers. The Rust side has no shared mutable state — only atomic counters in the global allocator (lib.rs:59-61) which are thread-safe. Each Go-side `ConvertBytesSlice` creates its own local `runtime.Pinner`, local `items` array, and local CGo call, making concurrent invocation safe.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:368-377` — `batchConvertTransactionsToJSON` called once per page after all transactions accumulated; runs sequentially in the request goroutine
- `cmd/stellar-rpc/internal/methods/json.go:105-210` — six sequential `ConvertBytesSlice` calls at lines 122, 126, 130, 144, 158, 179; each operating on independent slices (results, envelopes, metas, diagEvents, txEvents, contractEvents)
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:65-132` — `ConvertBytesSlice` creates local `runtime.Pinner`, builds local `[]C.xdr_t` items array, makes single CGo call, copies results; no shared state between invocations
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:208-289` — `xdr_batch_to_json` only accesses function-local data; type resolution via `TypeVariant::from_str` is stateless; per-item loop allocates owned `ConversionResult` structs with no cross-batch sharing
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:59-65` — global `CountingAllocator` uses `AtomicBool`/`AtomicUsize`, thread-safe for concurrent allocation

### Findings

The inefficiency is real and is in a hot path. Unlike fail/005 (which targeted fixed per-call overhead of ~0.5-1μs), this hypothesis targets the **data-proportional work** — XDR deserialization + JSON serialization — which fail/005 itself identified as the dominant cost at ~12-220μs per item.

**Per-batch cost estimates for a 200-transaction Soroban-heavy page:**
- TransactionResult (200 items, ~100-500B each): ~2-5ms
- TransactionEnvelope (200 items, ~500-2KB each): ~3-8ms
- TransactionMeta (200 items, ~1-100KB each): ~15-40ms
- DiagnosticEvents (hundreds-thousands of events): ~5-30ms
- TransactionEvents (variable): ~3-15ms
- ContractEvents (variable): ~3-15ms

**Sequential total**: ~30-113ms
**Parallel (max of 6 batches)**: ~15-40ms (dominated by metas)
**Savings**: ~15-73ms

**Total request time**: ~50-200ms (includes DB read, transaction iteration, JSON conversion, response serialization)
**Conversion fraction**: ~30-60% of total request time
**Net latency reduction**: ~10-35% for Soroban-heavy pages; ~5-15% for mixed workloads

The fix is straightforward: dispatch the six `ConvertBytesSlice` calls as goroutines with `sync.WaitGroup` or `errgroup.Group`, join before the assignment loop. No new FFI contracts needed. Correctness is preserved because each goroutine writes to its own output slice variable and the assignment loop runs after all goroutines complete.

**Risks are manageable**: CGo calls lock an OS thread for their duration, so 6 concurrent calls use 6 OS threads. Under high concurrent request load, this increases thread pressure. A bounded semaphore or `GOMAXPROCS`-aware fan-out (e.g., `min(6, runtime.GOMAXPROCS(0))`) could cap thread usage.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/methods/json.go:105-210` — `batchConvertTransactionsToJSON`
- **Change description**: Replace the six sequential `ConvertBytesSlice` calls (lines 122-179) with concurrent goroutines using `errgroup.Group` (from `golang.org/x/sync/errgroup` or a simple `sync.WaitGroup` + error channel). Each goroutine captures its input slice and writes to its dedicated output variable. The assignment loop (lines 184-208) runs unchanged after `errgroup.Wait()`. Use a bounded `errgroup` with `SetLimit(min(6, runtime.GOMAXPROCS(0)))` to avoid oversubscribing CPUs.
- **Correctness check**: Existing tests for `getTransactions` with `format=json` cover the output correctness. The race detector (`go test -race`) should be run to verify no data races on the shared `pending` slice (read-only) or output variables (written by one goroutine each).
- **Benchmark focus**: Measure `getTransactions` p50/p99 latency for 200-transaction pages with Soroban-heavy content. Target metric: wall-clock time of `batchConvertTransactionsToJSON` should drop from sum-of-batches to approximately max-of-batches (expected ~2-4× reduction in conversion time, ~10-35% reduction in end-to-end latency).
