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

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated (prior fail files cover batch call boundaries and type dispatch, not per-item reader construction cost)
**Failed At**: reviewer

### Trace Summary

Traced the per-item deserialization path inside `xdr_batch_to_json` (lib.rs:231-239) through `Limited::new` (generated.rs:384-386) and `Type::read_xdr_to_end` (generated.rs:57579-57587). Confirmed that `Limited::new` is a trivial struct initialization copying two integers (`depth: u32`, `len: usize`) plus a slice reference — no heap allocation, no syscall, ~2-3ns. The EOF check in `read_xdr_to_end` does `r.read(&mut [0u8; 1])` on a `&[u8]`, which is a simple bounds check returning 0 for an exhausted slice — ~1-2ns. Then examined the proposed alternative `read_xdr_iter` (generated.rs:458-510, 57608-57680) and found it would be strictly MORE expensive: it allocates a `BufReader` (8KB internal buffer), wraps the result in a `Box<dyn Iterator>` (heap allocation + dynamic dispatch), and performs a `fill_buf()` call before every item.

### Code Paths Examined

- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:231-239` — batch loop: `slice::from_raw_parts` (no-op pointer cast), `Limited::new(xdr_slice, DEFAULT_XDR_RW_LIMITS.clone())` (copies ~24 bytes to stack), `Type::read_xdr_to_end` (deserialize + 1-byte EOF read)
- `stellar-xdr-26.0.0/src/curr/generated.rs:373-386` — `Limited` struct: two fields (`inner: L`, `limits: Limits`); `new()` is trivial field assignment, no allocation
- `stellar-xdr-26.0.0/src/curr/generated.rs:326-339` — `Limits` struct: `depth: u32` + `len: usize`; `.clone()` copies 12 bytes (or 16 with padding); `DEFAULT_XDR_RW_LIMITS` is a `const`, so cloning is just a register/stack copy
- `stellar-xdr-26.0.0/src/curr/generated.rs:569-578` — `read_xdr_to_end`: calls `read_xdr(r)` then `r.read(&mut [0u8; 1])` to verify EOF; for `&[u8]` reader the EOF check is a length comparison returning 0
- `stellar-xdr-26.0.0/src/curr/generated.rs:424-429` — `Read for Limited<R>`: delegates directly to `self.inner.read(buf)` — zero overhead wrapper
- `stellar-xdr-26.0.0/src/curr/generated.rs:458-510` — `ReadXdrIter`: wraps reader in `BufReader::new(r)` (allocates 8KB buffer), returns `Box<dyn Iterator>` (heap allocation); each `next()` calls `fill_buf()` then `with_limited_depth` then `read_xdr`

### Why It Failed

The per-item "reader setup" cost claimed by the hypothesis is essentially zero:

1. **`Limited::new` cost**: Copies a `&[u8]` slice (pointer + length = 16 bytes) and a `Limits` struct (u32 + usize = 12 bytes) onto the stack. `DEFAULT_XDR_RW_LIMITS` is a `const`, so `.clone()` is a compile-time-known value assignment. Total: ~2-3ns, no heap allocation.

2. **EOF check cost**: `r.read(&mut [0u8; 1])` on a `&[u8]` that has been fully consumed by `read_xdr` returns 0 immediately (the slice length is 0). This is a single branch, extremely well-predicted (always true in success case). Total: ~1-2ns.

3. **Combined per-item overhead**: ~3-5ns per item. For 1000 events: ~3-5μs. For a 50-200ms request: **0.0025-0.01%**.

4. **Proposed alternative is WORSE**: `read_xdr_iter` wraps the reader in `BufReader::new(r)` which allocates an 8KB internal buffer, returns a `Box<dyn Iterator>` (heap allocation + vtable indirection), and each `next()` call performs a `fill_buf()` before reading. Additionally, the hypothesis requires concatenating all items into a single contiguous buffer first, which means an extra allocation of `sum(item_sizes)` bytes plus copying every item. The total overhead of the proposed alternative significantly exceeds the ~3-5ns per-item cost it attempts to eliminate.

5. **XDR items are NOT concatenation-safe by default**: XDR items in the batch have independent pointer/length pairs because they come from separate database rows. Concatenating them requires knowing that each item's serialized form contains exactly the right number of bytes with no padding or framing — which is true for XDR but creates a fragile coupling. The current design where each item is independently bounded is more robust and costs essentially nothing.

### Lesson Learned

When evaluating "per-item overhead" in a hot loop, inspect the actual implementation cost of the operations, not just their API surface. `Limited::new` looks like "constructing a reader" but is actually a trivial field copy (~3ns). The `read_xdr_to_end` EOF check looks like "an extra I/O operation" but for `&[u8]` is just a length comparison (~2ns). Always check whether the proposed alternative actually reduces cost — `read_xdr_iter` would add `BufReader` allocation, `Box<dyn Iterator>` heap allocation, and dynamic dispatch, making it strictly worse than the current approach for this use case.
