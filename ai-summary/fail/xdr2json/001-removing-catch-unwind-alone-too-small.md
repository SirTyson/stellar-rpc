# H001: Removing `catch_unwind` Alone Does Not Materially Speed getTransactions

**Date**: 2026-04-07
**Subsystem**: xdr2json
**Severity**: Low
**Impact**: latency / CPU
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

The per-call panic guard in the `xdr2json` FFI layer should add only a small fixed cost relative to the actual XDR deserialization and JSON serialization work. Any optimization that weakens that guard would only be justified if it delivered a clearly measurable `getTransactions` latency win while preserving the requirement that panics never unwind across the FFI boundary.

## Mechanism

`xdr_to_json` wraps every conversion in `catch_json_to_xdr_panic(Box::new(move || { ... }))`, which introduces a boxed closure, dynamic dispatch, and unwind setup on every call. That looks like hot-path overhead at first glance, especially when `getTransactions` performs thousands of conversions on event-heavy pages.

## Trigger

1. Issue `getTransactions` with `format=json` against event-heavy ledgers.
2. Compare current profiles against a prototype that only refactors the panic wrapper shape while leaving the rest of the conversion path unchanged.
3. Measure whether the wrapper alone moves total request latency or allocations by a meaningful amount.

## Target Code

- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:65-84` — `xdr_to_json` wraps the core logic in a boxed closure.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:116-140` — `catch_json_to_xdr_panic` performs the unwind boundary.

## Evidence

The wrapper is undeniably present on every conversion, and unlike the actual XDR parse it does not contribute directly to the returned JSON payload. That makes it a plausible fixed-cost suspect when reading the FFI hot path.

## Anti-Evidence

The surrounding path still does much heavier work per call: input copies, XDR decoding, JSON serialization, CString creation, and Go-side result copying. More importantly, the panic boundary is required for FFI safety, so the practical optimization space is limited to tiny structural refactors rather than removal.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The panic wrapper is safety-critical and its standalone cost is too small relative to the bulk copy and serialization work in the same call chain to plausibly deliver a measurable `getTransactions` improvement by itself.

### Lesson Learned

For this subsystem, the worthwhile optimizations are the ones that remove full-buffer copies or repeated per-item allocations. Nanosecond-scale safety scaffolding is not a strong hypothesis unless it is coupled to a larger structural change such as batching.
