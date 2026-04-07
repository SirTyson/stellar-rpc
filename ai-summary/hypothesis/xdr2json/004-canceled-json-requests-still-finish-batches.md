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
