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

---

## Review

**Verdict**: VIABLE
**Severity**: Medium
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full `xdr_batch_to_json` function (lib.rs:208-289) and confirmed the sequential `for item in items_slice` loop at lines 231-258. Each iteration performs `slice::from_raw_parts` → `xdr::Limited::new` → `Type::read_xdr_to_end` → `serde_json::to_string` with zero cross-item data dependencies. Verified that no threading library (Rayon, crossbeam) exists anywhere in the workspace Cargo.lock. Also confirmed `CXDR` (ffi/src/lib.rs:9-12) contains `*mut c_uchar` making it `!Send`, but items are only read-accessed via `from_raw_parts` as immutable slices, so a pre-conversion to `&[u8]` slices before the parallel region would cleanly resolve the Send constraint.

### Code Paths Examined

- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:208-289` — `xdr_batch_to_json`: sequential loop over `items_slice`, each iteration does `read_xdr_to_end` + `serde_json::to_string` independently; `Vec::with_capacity(count)` pre-allocates but items are pushed sequentially
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:221-226` — type resolution via `TypeVariant::from_str` happens once before the loop (stateless, `Copy` enum); no per-item type overhead
- `cmd/stellar-rpc/lib/ffi/src/lib.rs:8-12` — `CXDR` struct: `*mut c_uchar` + `size_t`, `Copy + Clone` but `!Send` due to raw pointer; items only ever read-accessed in the batch loop
- `cmd/stellar-rpc/internal/methods/json.go:105-210` — `batchConvertTransactionsToJSON`: 6 sequential `ConvertBytesSlice` calls, batch sizes are 200 for core fields and hundreds-thousands for events
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:65-132` — `ConvertBytesSlice`: single CGo crossing per batch; Go pins memory via `runtime.Pinner`; Rust call is synchronous
- `cmd/stellar-rpc/lib/xdr2json/Cargo.toml` — no threading dependencies; Rayon would need to be added

### Findings

**The inefficiency is real and in a hot path.** The batch loop is strictly sequential despite items being embarrassingly parallel. Each `read_xdr_to_end` + `serde_json::to_string` is CPU-bound with zero shared state.

**Per-batch cost estimates for a 200-tx Soroban-heavy page (sequential):**
- TransactionResult (200 items, ~100-500B XDR): ~3-5ms
- TransactionEnvelope (200 items, ~500-2KB XDR): ~4-8ms
- TransactionMeta (200 items, ~1-100KB XDR): ~15-40ms ← bottleneck batch
- DiagnosticEvents (hundreds-thousands): ~5-30ms
- TransactionEvents (variable): ~3-15ms
- ContractEvents (variable): ~3-15ms

**Sequential total**: ~33-113ms across 6 batches
**With 8-core Rayon parallelism per batch** (still 6 sequential batches from Go): ~6-18ms
**Savings**: ~27-95ms

**Total request time**: ~50-200ms (includes DB read, Go XDR unmarshalling, conversion, response)
**Conversion fraction**: ~30-60% of total
**Net latency reduction**: ~15-50% for Soroban-heavy pages; ~5-15% for mixed workloads

**Severity downgrade rationale (High → Medium):** The hypothesis claims High (>20% latency reduction), which is accurate for Soroban-heavy workloads on 8+ core machines. However, typical cloud deployments use 2-4 cores, which limits the Rayon speedup to ~2-3.5× per batch. On a 4-core machine, the metas batch drops from ~20ms to ~6ms (saving ~14ms), and combined batch savings are ~15-40ms on a 50-150ms request — typically ~10-25%. Additionally, if H001 (Go-level parallelism of 6 batches) is also implemented, the marginal benefit of Rust-level parallelism decreases since the bottleneck narrows to the single largest batch. The practical improvement in common deployment scenarios places this in the Medium range.

**Implementation considerations:**
1. **`CXDR` is `!Send`**: The `*mut c_uchar` makes `CXDR` non-sendable. The cleanest approach is to convert all items to `&[u8]` slices (which are `Send + Sync`) before entering the parallel region, rather than using unsafe `Send` wrappers.
2. **Rayon is a new dependency**: Not currently in the workspace. Adding it introduces a thread pool that persists for the process lifetime. The global pool is shared across all concurrent CGo calls, which is correct — Rayon's work-stealing scheduler handles multiple concurrent `par_iter` callers naturally.
3. **Threshold needed**: Small batches (<32-64 items) should fall back to sequential to avoid thread scheduling overhead. Rayon's `par_iter` has some minimum chunk logic, but an explicit threshold is cleaner.
4. **Panic handling**: The current `panic::catch_unwind` wraps the entire loop. With Rayon, panics in worker threads need per-item `catch_unwind` or Rayon's built-in panic propagation.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:221-261` — the `panic::catch_unwind` closure inside `xdr_batch_to_json`
- **Change description**: Add `rayon` to `cmd/stellar-rpc/lib/xdr2json/Cargo.toml`. Inside the `catch_unwind` closure, after type resolution (line 226), add a threshold check: if `count > 64`, convert `items_slice` to a `Vec<&[u8]>` of immutable slices, then use `rayon::iter::IntoParallelIterator::into_par_iter` with `.enumerate().map(|(i, xdr_slice)| { ... }).collect::<Vec<ConversionResult>>()` to produce indexed results in parallel. For `count <= 64`, keep the existing sequential loop. The `map` closure body is the same as the current loop body (lines 232-257) but returns a `ConversionResult` instead of pushing to a Vec.
- **Correctness check**: Existing `getTransactions` JSON tests + Rust-side `cargo test` in the xdr2json crate. Run `cargo test` with `--release` to match production build. The Rayon `par_iter().collect()` preserves input order, so output order is maintained.
- **Benchmark focus**: Measure wall-clock time of `xdr_batch_to_json` for TransactionMeta batches of 200 items (the bottleneck batch). Target: ~3-4× speedup on 4-core, ~5-7× on 8-core. Also measure overall `getTransactions` p50/p99 latency for 200-tx Soroban-heavy pages. Target: ~10-25% end-to-end latency reduction on 4-core machines.
