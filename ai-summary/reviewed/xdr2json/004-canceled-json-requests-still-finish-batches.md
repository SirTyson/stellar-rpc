# H004: Canceled JSON Requests Still Run the Full xdr2json Batch Pipeline

**Date**: 2026-04-07
**Subsystem**: xdr2json
**Severity**: Medium
**Impact**: CPU / throughput / tail latency
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When a `getTransactions` request times out or the client disconnects, the server
should stop expensive JSON conversion as soon as practical. It should not keep
parsing and serializing an already-abandoned page through all remaining xdr2json
batches.

## Mechanism

`getTransactionsByLedgerSequence()` checks `ctx.Err()` while reading ledgers and
transactions, but once it reaches `batchConvertTransactionsToJSON()` the context is
no longer consulted. The batch helper always runs up to six `ConvertBytesSlice()`
calls, and neither `ConvertBytesSlice()` nor `xdr_batch_to_json()` accepts a
context or a cancellation checkpoint. Under request-duration limits or client
abort, a large JSON page can therefore keep consuming CPU for metas and event
batches whose outputs will never be written.

## Trigger

1. Start a large `getTransactions` request in `format=json`.
2. Cancel the client request or force a short server deadline after the page has
   been collected but before batch conversion completes.
3. Observe whether CPU continues in `batchConvertTransactionsToJSON()` and
   `xdr_batch_to_json()` despite the request context already being done.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:345-376` — context is
  checked during ledger scanning, then the handler unconditionally enters
  `batchConvertTransactionsToJSON()`.
- `cmd/stellar-rpc/internal/methods/json.go:105-210` — the batch helper has no
  `context.Context` parameter and no cancellation checks between the six batch calls.
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:65-133` — `ConvertBytesSlice()`
  exposes no way to abort long-running batches.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:208-289` — `xdr_batch_to_json()`
  processes the whole batch once started.

## Evidence

The handler explicitly propagates `ctx` through database reads and checks it inside
the per-ledger transaction loop, which shows the endpoint is expected to stop on
cancellation. That expectation stops at the batch JSON phase: the conversion path
has no context parameter, no checkpoint between homogeneous groups, and no chunking
that would let the handler bail out once the request is already dead.

## Anti-Evidence

This does not improve successful-request latency directly, and a single FFI call
cannot be interrupted mid-batch without a larger ABI change. The measurable win is
workload-dependent: it matters most when deadlines are short or clients abandon
large JSON pages under load.

---

## Review

**Verdict**: VIABLE
**Severity**: Informational
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

The `getTransactionsByLedgerSequence` handler checks `ctx.Err()` inside the chunk-processing loop (get_transactions.go:346) and inside `processTransactionsInLedger` (get_transactions.go:118), but after the loop exits, line 371 enters `batchConvertTransactionsToJSON` with no prior ctx check and no context parameter. The batch function makes 6 sequential CGo calls (3 core field batches + 3 event-type batches) with zero cancellation checkpoints between them. The server enforces a 5-second `MaxGetTransactionsExecutionDuration` timeout via `httpRequestDurationLimiter` which cancels the request context, but this cancellation goes unobserved during the batch conversion phase.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:345-376` — ctx.Err() checked at line 346 inside the chunk loop; no check before or during batchConvertTransactionsToJSON at line 371
- `cmd/stellar-rpc/internal/methods/get_transactions.go:78-120` — processTransactionsInLedger checks ctx.Err() at line 118 inside per-transaction loop
- `cmd/stellar-rpc/internal/methods/json.go:105-210` — batchConvertTransactionsToJSON has no context.Context parameter; makes 6 sequential ConvertBytesSlice/jsonifySlice calls with no cancellation checkpoints
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:65-133` — ConvertBytesSlice is a synchronous CGo call with no abort mechanism
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:139-153` — httpRequestDurationLimiter creates context.WithTimeout and cancels on threshold
- `cmd/stellar-rpc/internal/config/options.go:576-579` — MaxGetTransactionsExecutionDuration defaults to 5 seconds

### Findings

The inefficiency is confirmed: the batch conversion phase (up to 6 CGo calls processing results, envelopes, metas, diagnostic events, transaction events, and contract events for an entire page) runs to completion even when the request context has been canceled. The server-side 5-second timeout via `httpRequestDurationLimiter` cancels the context, but this signal is not checked between batch calls.

However, **the severity is Informational, not Medium**, for two reasons:

1. **Zero impact on successful requests**: The ctx.Err() check adds negligible overhead to normal requests. But conversely, for requests that complete within the deadline, this optimization produces zero latency or throughput improvement.

2. **Impact is workload-dependent and unmeasurable in standard benchmarks**: The benefit only manifests when requests are actively being canceled during the batch conversion window. Under normal benchmarks (all successful requests), this produces no measurable change. The improvement depends entirely on: (a) what fraction of requests time out, (b) how often the timeout fires specifically during the batch phase (vs during DB reads or chunk processing), and (c) how much batch work remains after cancellation.

The fix is trivially correct: add `context.Context` as a parameter to `batchConvertTransactionsToJSON` and check `ctx.Err()` before each of the 6 CGo calls. Individual CGo calls cannot be interrupted mid-execution, but early exit between calls is safe since each call is independent.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/methods/json.go:105-210` — add `ctx context.Context` parameter to `batchConvertTransactionsToJSON`; check `ctx.Err()` before each `ConvertBytesSlice`/`jsonifySlice` call. Also update the call site at `get_transactions.go:371` to pass `ctx`.
- **Change description**: Thread `context.Context` through `batchConvertTransactionsToJSON` and insert 5 cancellation checkpoints (before calls 2-6; the first call can proceed unconditionally since we just entered the function). Return `ctx.Err()` if the context is done.
- **Correctness check**: Existing `go-test` suite covers getTransactions; no tests should break since successful requests are unaffected. The only behavioral change is early return on canceled requests.
- **Benchmark focus**: Cannot be measured with a standard latency/throughput benchmark. Would require a stress test with concurrent requests and aggressive timeouts to demonstrate CPU savings on canceled requests. The metric to watch is aggregate CPU utilization under overload, not per-request latency.
