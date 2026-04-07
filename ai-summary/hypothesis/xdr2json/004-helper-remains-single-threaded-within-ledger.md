# H004: `extract_transactions_json` Still Processes Every Transaction in a Ledger on One Core

**Date**: 2026-04-07
**Subsystem**: xdr2json
**Severity**: High
**Impact**: latency / CPU / multicore utilization
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

After a raw `LedgerCloseMeta` has been parsed, per-transaction JSON extraction
should exploit available CPU cores for dense ledgers. A large JSON page should not
serialize every transaction’s result/meta/envelope/event set through one Rust loop
when each transaction is independent and output order can be preserved by index.

## Mechanism

`extract_transactions_json()` iterates `for i in 0..count` and does all per-tx
work serially: hash formatting, fee-bump/success checks, `serde_json::to_value`
for result/meta/envelope, event extraction, and object assembly. For Soroban-heavy
ledgers this is CPU-bound and embarrassingly parallel because each iteration reads
immutable `tx_set[i]` / `processing[i]` state and produces its own transaction
object. Parallelizing the per-tx map with ordered collection would reduce wall
time toward the slowest chunk instead of the full sum.

## Trigger

1. Issue `getTransactions` with `format=json` against dense ledgers containing
   100+ transactions with large metas and events.
2. Profile the Rust helper; expect hot samples in `serde_json::to_value`,
   `extract_events`, and the per-tx loop body.
3. Compare against a prototype that parallelizes the per-ledger transaction map
   with a bounded worker pool or Rayon while preserving index order.

## Target Code

- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:505-556` — one sequential loop handles every transaction in the ledger.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:433-473` — `extract_events()` performs independent per-tx event conversion work.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:394-428` — envelope extraction produces immutable references that can be read concurrently.

## Evidence

The helper precomputes `tx_set` and `processing`, then each iteration only reads
`tx_set[i]`, `processing[i]`, and derived event collections. There is no shared
mutable transaction-level state beyond pushing into `result_array`, which can be
replaced by indexed collection.

## Anti-Evidence

Small ledgers will not amortize thread orchestration overhead, so any fix needs a
size threshold. If Go also parallelizes across ledgers, worker-count limits will
matter to avoid oversubscribing busy hosts.
