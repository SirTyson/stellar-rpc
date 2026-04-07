# H011: Swapping `serde_json::to_string` for `to_vec` / `to_writer` Alone Is Too Small

**Date**: 2026-04-07
**Subsystem**: xdr2json
**Severity**: Low
**Impact**: latency / allocations
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

If the Rust serializer entrypoint is a meaningful `getTransactions` bottleneck on
its own, replacing `serde_json::to_string(...).into_bytes()` with `to_vec()` or a
trivial `to_writer()` refactor should remove a clearly identifiable payload copy
or allocation from the hot path. The serializer swap would need to change the
actual output data movement, not just the API surface.

## Mechanism

At first glance, `serde_json::to_string(&t).unwrap().into_bytes().into_boxed_slice()`
looks wasteful because it mentions both `String` and bytes. But `String::into_bytes()`
moves the existing buffer without copying, and xdr2json still needs an owned
buffer to return over FFI even if `to_vec()` is used instead. That leaves only
small differences in serializer internals or initial capacity growth, not a clear
elimination of a major hot-path copy.

## Trigger

1. Inspect the single-item and batch success paths in `lib.rs`.
2. Compare the current `to_string(...).into_bytes()` chain with a hypothetical
   `to_vec()` or `to_writer(&mut Vec<u8>)` refactor.
3. Look for a removed allocation or memcpy that scales with `getTransactions`
   payload size.

## Target Code

- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:xdr_to_json:151-166` — single-item path serializes to `String` and then moves into boxed bytes.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:xdr_batch_to_json:235-255` — batch path uses the same `serde_json::to_string(...).into_bytes()` pattern per item.

## Evidence

The current code does create a `String` first, so it is natural to ask whether a
byte-oriented serializer API would remove that layer. The code path is also used
for every successful conversion, so even a moderate improvement would matter.

## Anti-Evidence

`String::into_bytes()` is already a move of the existing allocation, not a second
payload copy, and the FFI result still needs owned bytes regardless of whether
the serializer started from `String` or `Vec<u8>`. Without a change to batching,
buffer reuse, or return layout, this is mostly an API-shape swap.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

I could not identify a concrete payload-sized copy or allocation that disappears
from the current xdr2json success path by switching from `to_string` to `to_vec`
alone. The owned output buffer is still required for the FFI return, and
`into_bytes()` already transfers ownership of the serialized bytes without a copy.

### Lesson Learned

For this subsystem, serializer API substitutions are only interesting when they
pair with a larger change such as buffer reuse, arena layout, or batch-level
output reshaping. If the bytes still have to be owned and returned individually,
changing `to_string` to `to_vec` by itself is mostly cosmetic.
