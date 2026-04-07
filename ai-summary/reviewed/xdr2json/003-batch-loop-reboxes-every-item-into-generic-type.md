# H003: `xdr_batch_to_json` Still Reboxes Every Batch Item Into `stellar_xdr::Type`

**Date**: 2026-04-07
**Subsystem**: xdr2json
**Severity**: Medium
**Impact**: latency / CPU / allocations
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once the batch path has resolved a homogeneous type such as `DiagnosticEvent`,
`ContractEvent`, or `TransactionEvent`, each item should deserialize straight into
that concrete Rust type and serialize from there. The batch loop should not keep
routing every item through the generic `stellar_xdr::Type` enum and a fresh boxed
wrapper when the endpoint only exercises a tiny fixed subset of hot types.

## Mechanism

`xdr_batch_to_json()` resolves `TypeVariant` once per batch, but inside the item
loop it still calls `xdr::Type::read_xdr_to_end(the_type, &mut buffer)`. The
generated `Type::read_xdr()` implementation is a giant match over hundreds of
variants and returns `Type::{...}(Box<ConcreteType>)`, so every event in a hot
batch pays a dynamic dispatch plus one extra heap box before `serde_json` sees
it. A hot-type-specialized batch path (for the six `getTransactions` types, or at
least the three event batch types) could deserialize directly as
`DiagnosticEvent`, `ContractEvent`, or `TransactionEvent` and serialize those
values without the generic wrapper.

## Trigger

1. Issue `getTransactions` with `xdrFormat=json` on pages containing thousands of
   small event items.
2. Profile the Rust batch loop and allocation hot spots, focusing on
   `Type::read_xdr_to_end`, `Box::new(...)`, and per-item enum construction.
3. Compare against a prototype that matches `the_type` once outside the loop and
   then calls a typed helper for the concrete reader/serializer pair.

## Target Code

- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:xdr_batch_to_json:221-258` — per-item loop currently deserializes into the generic `Type` wrapper.
- `/home/garand/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/stellar-xdr-26.0.0/src/curr/generated.rs:Type:54101-54568` — `Type` stores every decoded payload behind a boxed enum variant.
- `/home/garand/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/stellar-xdr-26.0.0/src/curr/generated.rs:Type::read_xdr:55516-57576` — generated deserializer selects the concrete reader via a giant variant match.
- `cmd/stellar-rpc/internal/methods/json.go:jsonifySlice/jsonifySliceOfSlices:56-90` — `getTransactions` feeds the hot homogeneous event batches into this generic Rust path today.

## Evidence

The batch API already knows all items share one XDR type, yet the implementation
still constructs the most general representation (`Type`) for each item. The hot
`getTransactions` event families are especially attractive because they hit this
loop many times with small payloads, where fixed per-item dispatch and box
allocation are a larger fraction of total work.

## Anti-Evidence

For large `TransactionMeta` or large event payloads, the concrete XDR decode and
JSON serialization may still dominate enough that wrapper removal only yields a
small win. A maintainable fix likely needs macro-generated specializations or a
carefully limited set of hot-type arms so the Rust code does not become brittle.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated (fail/003 covers type-string dispatch overhead, not per-item Box allocation in the batch loop)

### Trace Summary

Traced the full batch conversion path from Go `batchConvertTransactionsToJSON` (json.go:105-211) through `ConvertBytesSlice` → CGo → `xdr_batch_to_json` (lib.rs:208-289). Confirmed every item in the batch loop (lib.rs:231-258) calls `xdr::Type::read_xdr_to_end(the_type, &mut buffer)` which dispatches through a 468-arm match (generated.rs:55516-57576) and wraps the result in `Box::new(ConcreteType)` (e.g., line 56473: `Box::new(DiagnosticEvent::read_xdr(r)?)`). The `Type` enum is `#[serde(untagged)]` (line 54098), so serialization delegates directly to the concrete type's `Serialize` impl — no enum tag overhead — but the `Box` allocation and 468-arm match dispatch occur per item. A specialized path calling `DiagnosticEvent::read_xdr_to_end(&mut buffer)` directly would eliminate both.

### Code Paths Examined

- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:221-261` — batch loop: `TypeVariant` resolved once (line 223), but `Type::read_xdr_to_end(the_type, &mut buffer)` called per item (line 235), wrapping each in `Box<ConcreteType>`
- `stellar-xdr-26.0.0/src/curr/generated.rs:54095-54101` — `Type` enum is `#[serde(untagged)]` with every variant `Box`-wrapped; confirmed at lines 54323 (`ContractEvent(Box<ContractEvent>)`), 54326 (`DiagnosticEvent(Box<DiagnosticEvent>)`), 54334 (`TransactionEvent(Box<TransactionEvent>)`)
- `stellar-xdr-26.0.0/src/curr/generated.rs:55516-57576` — `Type::read_xdr`: 468-arm match dispatches to `Box::new(ConcreteType::read_xdr(r)?)` for each variant; entire function spans ~2060 lines of generated code
- `stellar-xdr-26.0.0/src/curr/generated.rs:569-578` — `ReadXdr::read_xdr_to_end` is a provided trait method available on all concrete types, confirming specialization is feasible without reimplementing EOF checks
- `cmd/stellar-rpc/internal/methods/json.go:105-182` — Go batching: 6 `ConvertBytesSlice` calls per page (3 core fields + 3 event types), with event counts of 1000-4000+ items for Soroban-heavy ledgers

### Findings

The inefficiency is **real**: every batch item pays for one `Box::new()` heap allocation (~20-60ns alloc + ~15-30ns dealloc = ~35-90ns) plus a 468-arm match dispatch in both `Type::read_xdr` and the serde `Serialize` impl (branch-predicted after first iteration, but icache-unfriendly due to the ~2000-line function body).

The fix is **correct**: since `Type` uses `#[serde(untagged)]`, the JSON output of `serde_json::to_string(&Type::DiagnosticEvent(Box::new(event)))` is byte-identical to `serde_json::to_string(&event)`. All concrete types implement both `ReadXdr` (with `read_xdr_to_end` provided) and `serde::Serialize`, so a specialized match outside the loop can call directly to the concrete type.

**Impact estimate**: For a batch of 4000 small events (~100-byte XDR payloads):
- Per-item savings: ~45-90ns (Box alloc/dealloc + reduced match/icache overhead)
- Batch savings: ~180-360μs
- Batch total processing: ~8-20ms (XDR deser + JSON ser at ~2-5μs per small event)
- **Rust-side improvement: ~1-4%**
- End-to-end `getTransactions` improvement: <2% (CGo overhead, Go processing, DB reads dilute further)

**Severity downgrade rationale**: The hypothesis claims Medium (5-20%), but the Box allocation is small relative to per-item XDR deserialization + JSON serialization work. The Branch predictor handles the match dispatch efficiently after the first iteration. For large payloads (TransactionMeta), the savings are negligible as data processing dominates. The improvement is real but below the 5% Medium threshold.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/lib/xdr2json/src/lib.rs`, function `xdr_batch_to_json`, lines 231-258
- **Change description**: Add a match on `the_type` before the loop with specialized arms for the 6 hot `getTransactions` types (`DiagnosticEvent`, `ContractEvent`, `TransactionEvent`, `TransactionMeta`, `TransactionResult`, `TransactionEnvelope`). Each arm should call `ConcreteType::read_xdr_to_end(&mut buffer)` directly and serialize without the `Type` wrapper. A macro can reduce boilerplate:
  ```rust
  macro_rules! specialized_batch {
      ($the_type:expr, $items_slice:expr, $type_str:expr, $($variant:ident => $concrete:ty),+ $(,)?) => {
          match $the_type {
              $(TypeVariant::$variant => {
                  batch_convert_typed::<$concrete>($items_slice, $type_str)
              })+
              _ => batch_convert_generic($the_type, $items_slice, $type_str)
          }
      }
  }
  ```
  where `batch_convert_typed<T: ReadXdr + Serialize>` deserializes and serializes without Boxing.
- **Correctness check**: Existing tests in `lib.rs` (`borrowed_slice_avoids_extra_clone_for_large_diagnostic_event`); also `cargo test` in the xdr2json crate. Verify JSON output is byte-identical for all 6 types by comparing specialized vs. generic paths on the same input.
- **Benchmark focus**: Measure heap allocations per batch item (should drop by 1 per item) and batch processing time for 1000-4000 small `DiagnosticEvent` items. Expect ~1-4% improvement in Rust-side batch processing time; use the crate's `CountingAllocator` to verify allocation reduction.
