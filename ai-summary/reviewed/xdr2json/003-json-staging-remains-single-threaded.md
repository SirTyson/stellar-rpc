# H003: `getTransactions` Still Prepares Every xdr2json Input on One Goroutine

**Date**: 2026-04-07
**Subsystem**: xdr2json
**Severity**: Medium
**Impact**: latency / CPU / multicore utilization
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once the relevant ledgers are already materialized in memory, the JSON path should
prepare per-transaction XDR inputs in parallel when those preparations are
independent. A large `getTransactions` page should not serialize all
`MarshalBinary()` and event-extraction work through one goroutine before the first
xdr2json batch even starts.

## Mechanism

Inside `processTransactionsInLedger()`, every JSON transaction immediately calls
`db.ParseTransaction()` inline. `ParseTransaction()` then marshals the result,
meta, and envelope, calls `GetTransactionEvents()`, and marshals every diagnostic,
transaction, and contract event into `[][]byte` form. Those per-transaction
operations are CPU-bound and independent once `reader.Read()` has returned the
`ingestTx`, but the current loop does all of them serially before appending to the
page-level `pending` batch.

## Trigger

1. Issue `getTransactions` with `format=json` on a page spanning 100-200
   transactions.
2. Use Soroban-heavy metas so `ParseTransaction()` has many event marshals.
3. Compare current behavior against a prototype that reads transactions in order
   but farms `ParseTransaction()` work to a bounded worker pool and then restores
   page order before batch conversion.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:117-205` — the per-ledger
  transaction loop performs JSON preparation inline at lines 152-163.
- `cmd/stellar-rpc/internal/db/transaction.go:276-315` — `ParseTransaction()`
  marshals `Result`, `Meta`, and `Envelope`, then extracts transaction events.
- `cmd/stellar-rpc/internal/db/transaction.go:319-354` — `parseEvents()` performs
  per-event `MarshalBinary()` work for all event families.

## Evidence

`processTransactionsInLedger()` already computes the cheap scalar response fields
(`hash`, `applicationOrder`, `feeBump`, ledger info, status) separately from the
JSON payload bytes, so the expensive staging work is isolated in `ParseTransaction()`.
After `reader.Read()` returns, that staging step no longer mutates shared ledger
state and only produces owned Go slices destined for the later batch conversion.

## Anti-Evidence

The reader itself is sequential, so only the post-`Read()` work can be parallelized.
If Rust-side conversion dominates a given workload, Go-side staging parallelism may
only move the overall needle by 5-20%, especially on small pages or ledgers with
few Soroban events.

---

## Review

**Verdict**: VIABLE
**Severity**: Medium
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated. This targets Go-side staging parallelism, which is distinct from reviewed/001 (Go-level parallelism of 6 batch FFI calls) and reviewed/002 (Rust-level Rayon parallelism within each batch). The three optimizations target different pipeline stages and are complementary.

### Trace Summary

Traced the complete path from `getTransactions` handler (get_transactions.go:345-365) through the per-ledger loop in `processTransactionsInLedger` (get_transactions.go:117-207) into `db.ParseTransaction` (transaction.go:276-315) and `parseEvents` (transaction.go:319-354). Confirmed that `reader.Read()` is a cheap O(1) map lookup (ledger_transaction_reader.go:64-94) returning pre-computed data, while `ParseTransaction()` performs CPU-bound `MarshalBinary()` calls on 3 core XDR fields plus all events. Each `ParseTransaction()` invocation is fully independent: it reads from the shared `LedgerCloseMeta` (immutable) and its own `ingestTx` (per-call value), producing an owned `db.Transaction` struct with no cross-transaction dependencies.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:117-207` — sequential for-loop reads one tx, calls `ParseTransaction` inline (line 154), appends result to `pending` slice; limit check at line 204 exits early
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:64-94` — `Read()` is O(1): increments `readIdx`, looks up envelope in `envelopesByHash` map, assembles `ingest.LedgerTransaction` from `lcm` accessor methods — no CPU-bound work
- `cmd/stellar-rpc/internal/db/transaction.go:276-315` — `ParseTransaction(lcm, ingestTx)`: derives scalar metadata (cheap), then calls `MarshalBinary()` on `Result.Result`, `UnsafeMeta`, and `Envelope` (CPU-bound: re-serializes typed XDR structs to `[]byte`), then `GetTransactionEvents()` + `parseEvents()`
- `cmd/stellar-rpc/internal/db/transaction.go:319-354` — `parseEvents()`: iterates DiagnosticEvents, TransactionEvents, and OperationEvents (ContractEvents), calling `MarshalBinary()` on each — O(N) in event count, fully independent per-transaction
- `cmd/stellar-rpc/internal/methods/json.go:105-210` — `batchConvertTransactionsToJSON`: consumes `pending` slice after all staging completes; requires all `[]byte` slices alive simultaneously for pinning — confirms staging must fully complete before conversion begins

### Findings

**The inefficiency exists and is in a hot path.** The staging loop in `processTransactionsInLedger` is single-threaded despite `ParseTransaction()` calls being embarrassingly parallel. Each call operates on independent data and produces owned output.

**Per-transaction staging cost estimates (Soroban-heavy):**
- `MarshalBinary(TransactionResult)` (~100-500B): ~1-5μs
- `MarshalBinary(TransactionMeta)` (~1-100KB): ~10-100μs ← dominates
- `MarshalBinary(TransactionEnvelope)` (~500-2KB): ~2-10μs
- `GetTransactionEvents()` + `parseEvents()` (~20-50 events): ~50-150μs
- **Per-tx total**: ~65-265μs

**For a 200-transaction Soroban-heavy page:**
- **Sequential staging total**: ~13-53ms
- **8-core parallel staging**: ~2-7ms
- **Staging savings**: ~11-46ms

**Context from reviewed/001 batch conversion estimates:**
- Sequential Rust conversion: ~30-113ms
- Total request time: ~50-200ms

**Go staging as fraction of total**: ~10-30% (mid-range: ~20%)
**Net latency reduction from parallelized staging**: ~7-25% for Soroban-heavy pages; ~5-15% for mixed workloads

**This optimization is complementary with reviewed/001 and reviewed/002.** If all three are implemented, the pipeline transforms from:
1. Sequential staging (~30ms) → Sequential 6 batches (~70ms) = ~100ms
2. Parallel staging (~4ms) → Parallel 6 batches with Rayon (~8ms) = ~12ms

**Correctness is preserved:**
- `LedgerCloseMeta` is read-only during the loop (shared immutable)
- Each `ingestTx` is a per-goroutine value copy from `Read()`
- `ParseTransaction` creates all-new allocations, no shared mutable state
- `MarshalBinary()` on XDR types is a read-only serialization of the source struct
- Output ordering is preserved by pre-allocating indexed slots

**Implementation complexity is low:** The standard `errgroup.Group` pattern with bounded concurrency handles the fan-out/fan-in cleanly. The only structural change is splitting the loop into a read phase (sequential, cheap) and a parse phase (parallel).

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/methods/get_transactions.go:117-207` — `processTransactionsInLedger`
- **Change description**: Refactor the JSON branch to (1) read all eligible transactions first into a pre-allocated slice (sequential — `reader.Read()` is O(1)), (2) pre-allocate `txInfo` entries and `pending` slots with correct indices, (3) dispatch `db.ParseTransaction()` calls to an `errgroup.Group` with `SetLimit(min(runtime.NumCPU(), 8))`, where each goroutine writes to its pre-assigned slot. The limit check can still be applied after the read phase since `Read()` cost is negligible. After `errgroup.Wait()`, the existing `batchConvertTransactionsToJSON` path runs unchanged.
- **Correctness check**: Existing `getTransactions` tests with `format=json` verify output correctness. Run with `-race` to confirm no data races. The `lcm` struct must only be read (never written) during the parallel phase — verify `TxApplyProcessing`, `TransactionResultPair`, etc. are pure accessors.
- **Benchmark focus**: Measure wall-clock time of the staging phase (from first `ParseTransaction` to last) for 200-tx Soroban-heavy pages. Target: ~4-8× speedup on 4-8 core machines. Also measure overall `getTransactions` p50/p99 latency. Target: ~5-20% end-to-end latency reduction, additive with reviewed/001 and reviewed/002 gains.
