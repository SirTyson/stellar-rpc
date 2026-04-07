# H004: `extract_transactions_json` Still Processes Every Transaction in a Ledger on One Core

**Date**: 2026-04-07
**Subsystem**: xdr2json
**Severity**: High
**Impact**: latency / CPU / multicore utilization
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

After a raw `LedgerCloseMeta` has been parsed, per-transaction JSON extraction
should exploit available CPU cores for dense ledgers. A large JSON page should not
serialize every transaction's result/meta/envelope/event set through one Rust loop
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

---

## Review

**Verdict**: VIABLE
**Severity**: Medium
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full `getTransactions` JSON path: Go `processChunksJSON` (get_transactions.go:253) iterates ledger chunks sequentially, calling `LCMTransactionsToJSON` (conversion.go:45) per ledger, which pins Go bytes and calls `C.lcm_transactions_to_json` (lib.rs:356). The Rust FFI entry deserializes the LCM once, then `extract_transactions_json` (lib.rs:477) runs a sequential `for i in 0..count` loop performing 3× `serde_json::to_value` plus `extract_events` per transaction. Each iteration reads only immutable references (`tx_set[i]`, `processing[i]`) and produces an independent `serde_json::Value`. The loop body is embarrassingly parallel.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:253-336` — `processChunksJSON` iterates chunks sequentially, one FFI call per ledger; no Go-level parallelism across or within ledgers
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:45-79` — `LCMTransactionsToJSON` pins bytes, calls FFI, copies JSON result back, unmarshals into `[]LCMTransactionJSON`
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:356-382` — `lcm_transactions_to_json` FFI entry: deserializes LCM, calls `extract_transactions_json`, boxes result
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:477-557` — `extract_transactions_json`: the sequential per-tx loop with 3× `serde_json::to_value` + `extract_events` + JSON object assembly
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:433-473` — `extract_events`: per-transaction event extraction with additional `serde_json::to_value` calls for each event
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:410-429` — `extract_envelopes`: produces `Vec<&TransactionEnvelope>` (immutable references)

### Findings

1. **The inefficiency is real**: `extract_transactions_json` processes all transactions in a ledger on one thread. The per-tx body (lines 508-553) performs three heavyweight `serde_json::to_value` calls (result, meta, envelope) plus `extract_events` which does additional serialization. Each iteration reads only shared immutable state and produces an independent value — textbook embarrassingly parallel.

2. **This is the current hot path**: Unlike reviewed hypotheses 001-003 (which target the old `batchConvertTransactionsToJSON` / `processTransactionsInLedger` path), this function is the ACTIVE code path for `format=json` requests via `processChunksJSON` (get_transactions.go:464-471). The old path is skipped entirely when `format=json`.

3. **No existing parallelism**: Neither the Go layer (sequential `for _, chunk := range chunks`) nor the Rust layer uses any parallelism for this path. There is no Rayon dependency in `xdr2json/Cargo.toml`.

4. **Correctness is preserved by the proposal**: The XDR types from `stellar-xdr` are standard Rust structs (no `Rc`, `Cell`, or raw pointers) and are `Send + Sync`. `serde_json::to_value` is thread-safe. The `tx_set` and `processing` vectors contain shared references that can be read concurrently. Replacing the sequential loop with `(0..count).into_par_iter().map(|i| { ... }).collect::<Vec<_>>()` preserves order and correctness.

5. **Impact assessment**: For dense ledgers (50-200 Soroban transactions with large metas), per-tx serialization can take 0.5-2ms, totaling 25-400ms per ledger. With 4-thread Rayon, this drops to ~25-100ms. For a page spanning multiple dense ledgers, the savings are multiplied. However, for sparse ledgers (10-20 small transactions), the per-ledger time is already small (1-10ms), and parallelism overhead may negate gains. A threshold (e.g., count > 32) is needed.

6. **Severity downgraded to Medium**: The hypothesis claims High severity (>20% latency reduction). While dense-ledger scenarios could see >20% improvement in the Rust processing step, the total `getTransactions` latency includes DB I/O, Go JSON unmarshaling (conversion.go:74), and response serialization. For typical workloads mixing sparse and dense ledgers, the overall improvement is likely 5-20%.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/lib/xdr2json/src/lib.rs` — function `extract_transactions_json` (lines 505-556)
- **Change description**: Add `rayon` dependency to `cmd/stellar-rpc/lib/xdr2json/Cargo.toml`. Replace the sequential `for i in 0..count { ... result_array.push(tx_obj); }` loop with `let result_array: Vec<serde_json::Value> = (0..count).into_par_iter().map(|i| { ... tx_obj }).collect();`. Add a threshold: only use `par_iter` when `count > 32`, falling back to sequential `iter` for small ledgers.
- **Correctness check**: Existing tests in `lib.rs` (the `mod tests` block starting at line 592) cover LCM transaction JSON extraction. Run `cargo test` in the xdr2json crate. Also run `make go-test` to verify the Go integration path.
- **Benchmark focus**: Measure per-ledger `extract_transactions_json` wall time for ledgers with 50, 100, and 200 Soroban transactions. Target metric: wall-time reduction proportional to available cores (expect ~2-3× speedup on 4+ core machines for large ledgers). Also measure end-to-end `getTransactions` latency to quantify the fraction of total time this saves.
