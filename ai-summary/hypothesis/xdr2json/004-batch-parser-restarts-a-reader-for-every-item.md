# H004: Homogeneous Event Batches Still Rebuild a Fresh `Limited` Reader for Every Item

**Date**: 2026-04-07
**Subsystem**: xdr2json
**Severity**: Low
**Impact**: latency / CPU
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

For a homogeneous batch of small event XDR blobs, the Rust side should amortize
parser setup across the batch as much as possible. It should not repeatedly build
a new `Limited` reader and perform a separate end-of-input validation read for
every tiny event when the entire batch is already known up front.

## Mechanism

Inside `xdr_batch_to_json()`, each item creates its own borrowed slice, wraps it
in `xdr::Limited::new(...)`, and then calls `Type::read_xdr_to_end(...)`. The
generated `read_xdr_to_end()` helper performs an additional one-byte read to
verify EOF after every decode. On pages with thousands of small events, that
fixed reader setup and EOF-check path repeats once per item even though the batch
is homogeneous and could instead be walked with a concatenated stream plus
`read_xdr_iter()` or another iterator-style typed decoder.

## Trigger

1. Issue `getTransactions` with `xdrFormat=json` against ledgers containing many
   tiny event payloads.
2. Benchmark the current batch loop against a prototype that concatenates one
   event family into a single buffer and decodes it with `read_xdr_iter()` or an
   equivalent typed iterator.
3. Measure CPU samples around `Limited::new`, `read_xdr_to_end`, and the EOF check
   inside the generated XDR reader.

## Target Code

- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:xdr_batch_to_json:231-239` — constructs a fresh slice and `Limited` reader for every batch item.
- `/home/garand/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/stellar-xdr-26.0.0/src/curr/generated.rs:Type::read_xdr_to_end:57579-57587` — every item performs its own terminal EOF validation read.
- `/home/garand/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/stellar-xdr-26.0.0/src/curr/generated.rs:Type::read_xdr_iter:57608-57680` — generated iterator-based decoding already exists and suggests a stream-oriented alternative.
- `cmd/stellar-rpc/internal/methods/get_transaction.go:BuildEventsJSONFromTransaction:143-155` — the event families driving this batch path are already grouped by homogeneous type.

## Evidence

The current batch ABI reduces CGo crossings but still treats each item as a tiny
independent document at the Rust reader boundary. The generated XDR code exposes
iterator-based decoding that is not used here, which makes the repeated
single-item reader lifecycle a plausible remaining fixed cost on small-event
workloads.

## Anti-Evidence

This likely helps only workloads dominated by many small events; large metas or
large event payloads will still spend most time in actual deserialization and
JSON serialization. It also requires a new packing format or a more elaborate
batch input ABI because the current API passes a pointer/length pair per item.
