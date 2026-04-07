# H005: `processChunksJSON` Still Runs Independent Ledger FFI Conversions in Series

**Date**: 2026-04-07
**Subsystem**: xdr2json
**Severity**: Medium
**Impact**: latency / CPU / multicore utilization
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once `BatchGetLedgersBySequences()` has materialized the selected ledger chunks in
memory and the SQLite snapshot has been released, JSON extraction should use
available cores across independent ledgers. A page spanning many ledgers should
not wait for one ledger’s Rust conversion to finish before the next one starts.

## Mechanism

`getTransactionsByLedgerSequence()` releases the DB snapshot before CPU-heavy work,
then `processChunksJSON()` loops over `chunks` and invokes
`xdr2json.LCMTransactionsToJSON(chunk.Lcm)` strictly sequentially. Each ledger
conversion reads only its own raw bytes and produces ordered per-ledger JSON
results, so a bounded parallel fan-out across chunks could overlap independent
Rust parses and JSON extraction. This is especially attractive for sparse pages
that span many small ledgers where no single ledger has enough transactions to
benefit much from within-ledger parallelism.

## Trigger

1. Issue `getTransactions` with `format=json` against a sparse history where the
   requested page spans many ledgers with only a few transactions each.
2. Profile CPU usage while `processChunksJSON()` runs; expect one core active at a
   time during the ledger FFI loop.
3. Compare current wall time against a prototype that dispatches per-ledger
   `LCMTransactionsToJSON` calls concurrently and merges ordered results by ledger
   sequence.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:251-333` — `processChunksJSON()` iterates selected ledgers serially and calls `LCMTransactionsToJSON()` per chunk.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:432-446` — all chunks are already materialized and the DB snapshot is released before this CPU-bound phase begins.
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:45-79` — each helper call is synchronous and owns its own output buffer.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:351-377` — `lcm_transactions_to_json()` uses only function-local state.

## Evidence

The handler already has the full `chunks` slice in memory before entering
`processChunksJSON()`, and the Rust entrypoint does not share mutable batch state
across calls. That makes each per-ledger conversion a natural concurrency unit.

## Anti-Evidence

Pages dominated by one dense ledger will see little benefit from across-ledger
parallelism alone. The implementation must preserve response ordering and avoid
wasting work once the global limit is satisfied, so a naive fire-all-threads
approach would need bounds and cancellation handling.
