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
