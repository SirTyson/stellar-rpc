# H002: `xdr_batch_to_json` Leaves Large Homogeneous Batches Single-Threaded

**Date**: 2026-04-07
**Subsystem**: xdr2json
**Severity**: High
**Impact**: latency / CPU / multicore utilization
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

After `getTransactions` has already collapsed thousands of same-type event payloads
into one `xdr_batch_to_json()` call, the Rust side should exploit the fact that
each item is independent. Large event batches should not burn one core while the
rest of the machine sits idle.

## Mechanism

`xdr_batch_to_json()` resolves the type once, but then iterates `for item in
items_slice` and performs every XDR decode and JSON serialization on the same
thread. Diagnostic, transaction, and contract event batches are embarrassingly
parallel: each item only reads its own `CXDR`, writes its own `ConversionResult`,
and preserves ordering by index. Chunking large batches across worker threads
could materially cut the dominant per-item parse + `serde_json` cost on event-heavy
pages without changing the public response shape.

## Trigger

1. Issue `getTransactions` with `format=json` against ledgers that produce
   thousands of event items in one page.
2. Profile the Rust batch loop; expect CPU samples concentrated in
   `read_xdr_to_end` and `serde_json::to_string`.
3. Compare current runtime against a prototype that parallelizes large batches
   by index range and reassembles ordered `ConversionResult`s.

## Target Code

- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:208-289` — `xdr_batch_to_json()`
  currently processes every item sequentially inside one loop.
- `cmd/stellar-rpc/internal/methods/json.go:135-180` — `getTransactions` feeds
  exactly the large homogeneous event batches that make the one-threaded Rust loop
  expensive.
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:102-130` — Go already reduces
  each event family to one FFI call, so the remaining hot cost sits inside the
  Rust loop itself.

## Evidence

The batch ABI passes immutable pointer/length pairs and the Rust loop allocates a
fresh output record per item, so there is no obvious cross-item dependency beyond
preserving output order. The latest `getTransactions` path already flattened event
families page-wide, which concentrates the remaining CPU work into one large
function that is currently forced onto a single core.

## Anti-Evidence

Very small batches will lose to thread orchestration overhead, so any fix likely
needs a size threshold. If page-level Go concurrency is also introduced, the Rust
implementation will need careful worker-count limits to avoid oversubscribing busy
servers.
