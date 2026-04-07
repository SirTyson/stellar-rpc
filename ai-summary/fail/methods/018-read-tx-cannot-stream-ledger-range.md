# H002: Missing Tx-Scoped Ledger Streaming Keeps `getTransactions` Waiting for Full Batch Decode

**Date**: 2026-04-07
**Subsystem**: methods
**Severity**: Medium
**Impact**: latency / CPU / allocations
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

After opening a read snapshot, `getTransactions` should be able to stream ledgers in sequence order and stop immediately once the page is full. The first returned transaction should not have to wait for the DB layer to scan and unmarshal an entire batch of `LedgerCloseMeta` rows into memory first.

## Mechanism

The non-transactional `LedgerReader` already exposes `StreamLedgerRange`, but `LedgerReaderTx` has no tx-scoped streaming equivalent. Because `getTransactions` must keep one read snapshot for both `GetLedgerRange` and the ledger walk, it cannot use the streaming API and instead falls back to `BatchGetLedgerMetas`, which calls `Select` into `[]xdr.LedgerCloseMeta` and returns only after every row in the range has been decoded. That front-loads XDR unmarshal work, delays time-to-first-transaction, and keeps unused tail ledgers alive on the heap until the whole batch is materialized.

## Trigger

1. Use ledgers with large `LedgerCloseMeta` blobs (for example, ledgers carrying many events).
2. Issue `getTransactions` requests that usually fill from the first few ledgers in the scanned range.
3. Compare latency and peak heap against a version that adds `StreamLedgerRange` to `LedgerReaderTx` and processes each row as it is scanned.

## Target Code

- `cmd/stellar-rpc/internal/db/ledger.go:25-40` — `LedgerReader` supports `StreamLedgerRange`, but `LedgerReaderTx` does not.
- `cmd/stellar-rpc/internal/db/ledger.go:118-140` — `BatchGetLedgerMetas` materializes the full batch into `[]xdr.LedgerCloseMeta`.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:263-299` — the handler cannot start parsing transactions until the full batch has been returned.

## Evidence

The interface mismatch is explicit: the handler opens a read transaction specifically to preserve a single snapshot, then loses access to the only range-streaming primitive and is forced into eager batch materialization. Unlike a streaming cursor, the current code cannot overlap row scanning with transaction parsing or stop the DB decode early once `done` flips true.

## Anti-Evidence

If the request usually consumes the whole batch anyway, streaming mainly improves memory shape and time-to-first-byte rather than total work. Some SQL drivers already stream rows internally, so the biggest gain depends on how much of the current cost comes from `[]xdr.LedgerCloseMeta` heap materialization rather than the underlying SQLite scan itself.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the `LedgerReaderTx` interface (ledger.go:35-41) and confirmed it lacks `StreamLedgerRange`. The non-tx `ledgerReader.StreamLedgerRange` (ledger.go:191-217) uses `r.db.Query(ctx, sql)` which returns `*db.Rows` for row-by-row iteration. The `ledgerReaderTx` struct holds a `db.SessionInterface` (ledger.go:55-59) which does expose `Query(ctx, squirrel.Sqlizer) (*Rows, error)` (go-stellar-sdk/support/db/main.go:145), so adding a tx-scoped streaming method is technically straightforward. The current handler (get_transactions.go:241-301) uses `const batchSize = 50` and calls `BatchGetLedgerMetas` which materializes all rows via `l.tx.Select` before returning, wasting decode work for any ledgers beyond the page-fill point.

### Code Paths Examined

- `cmd/stellar-rpc/internal/db/ledger.go:35-41` — `LedgerReaderTx` interface: has `BatchGetLedgerMetas` and `BatchGetLedgers` but no streaming method
- `cmd/stellar-rpc/internal/db/ledger.go:55-59` — `ledgerReaderTx` struct holds `tx db.SessionInterface` which has `Query()` for row-by-row iteration
- `cmd/stellar-rpc/internal/db/ledger.go:118-140` — `BatchGetLedgerMetas`: issues `SELECT meta ... ORDER BY sequence ASC`, then `l.tx.Select(ctx, &results, query)` materializes all rows into `[]xdr.LedgerCloseMeta` before returning
- `cmd/stellar-rpc/internal/db/ledger.go:191-217` — `ledgerReader.StreamLedgerRange`: uses `r.db.Query(ctx, sql)` for row-by-row iteration with callback, confirming the pattern already exists for non-tx reads
- `cmd/stellar-rpc/internal/methods/get_transactions.go:241` — `const batchSize = 50` governs all batch fetches
- `cmd/stellar-rpc/internal/methods/get_transactions.go:263` — `readTx.BatchGetLedgerMetas(ctx, uint32(batchStart), uint32(batchEnd))` — the full-materialization call
- `cmd/stellar-rpc/internal/methods/get_transactions.go:289-297` — inner loop processes ledgers and breaks on `done`, but the batch is already fully decoded
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/support/db/main.go:131-158` — `SessionInterface` definition confirms `Query` method (line 145) is available on transaction sessions

### Findings

**The interface gap is real.** `LedgerReader` has `StreamLedgerRange` (row-by-row via `Query`), but `LedgerReaderTx` forces callers into `BatchGetLedgerMetas` (full-materialization via `Select`). The underlying `SessionInterface` supports both patterns, so the gap is an API omission, not a technical limitation.

**The fix is correct and safe.** A `StreamLedgerRange` on `ledgerReaderTx` would use `l.tx.Query(ctx, sql)` within the existing read transaction, preserving snapshot isolation. The pattern is already proven by the non-tx implementation. Missing-ledger detection would shift from count-based (`len(ledgers) != expectedCount`) to sequence-continuity checks during streaming, which is straightforward.

**Impact is bounded by the existing batch size of 50.** After success/001's optimization (which replaced per-ledger lookups with `BatchGetLedgerMetas` in batches of 50), the waste per batch is at most 49 ledger decodes. The streaming approach would eliminate this waste entirely. For dense ledgers where the page fills from the first 1-2 ledgers, savings are ~48 × per-ledger-decode-cost. For sparse scans that consume nearly every ledger in each batch, savings are negligible.

**Related work (reviewed/001-fixed-batch-overfetches-dense-pages)** addresses the same root problem via adaptive batch sizing (probe small first, then grow). Streaming is a more complete solution (always optimal, zero wasted decodes) but more invasive (requires restructuring the batch-iterate pattern to a streaming-callback pattern). The two approaches could also be combined.

**Severity downgrade from Medium to Low.** The batch is already bounded at 50 ledgers. In the best case (dense, small-limit), savings are up to ~48 wasted decodes per request. At ~500µs per decode, that's ~24ms — meaningful but only for a specific dense-page workload. In the common sparse-scan case (the primary target of success/001), nearly every ledger in each batch is consumed, so streaming saves almost nothing. The improvement is real but workload-dependent and likely <5% for the majority of requests.

### PoC Guidance

- **Target code**:
  - `cmd/stellar-rpc/internal/db/ledger.go` — add `StreamLedgerRange(ctx, start, end uint32, f StreamLedgerFn) error` to `LedgerReaderTx` interface and implement on `ledgerReaderTx` using `l.tx.Query(ctx, sql)` with row-by-row iteration (follow the pattern in `ledgerReader.StreamLedgerRange` lines 191-217)
  - `cmd/stellar-rpc/internal/methods/get_transactions.go` — replace the batch-iterate loop (lines 247-301) with a streaming approach: call `readTx.StreamLedgerRange(ctx, start, end, callback)` where the callback processes each ledger via `processTransactionsInLedger` and returns an error to stop when the page is full
  - `cmd/stellar-rpc/internal/methods/mocks.go` — add `StreamLedgerRange` to the mock `LedgerReaderTx` implementation
- **Change description**: Add a tx-scoped streaming primitive to `LedgerReaderTx` that uses `Query` instead of `Select`, then refactor `getTransactionsByLedgerSequence` to process ledgers one at a time via the streaming callback instead of materializing batches of 50. Missing-ledger detection should check sequence continuity as each row arrives (track expected sequence, compare to actual).
- **Correctness check**: Existing tests in `cmd/stellar-rpc/internal/methods/get_transactions_test.go` cover pagination, limits, cursor, JSON format, and error cases. The mock needs the new method. Also verify missing-ledger error handling is preserved.
- **Benchmark focus**: Compare p50/p99 latency for `getTransactions` with dense ledgers and small limits (limit=1, limit=10) where the page fills from the first 1-2 ledgers. Also measure peak heap per request. Expect modest improvement (<5%) on mixed workloads but potentially 10-20% on targeted dense-page-small-limit workloads.

---

## PoC Attempt

**Result**: POC_PASS
**Date**: 2026-04-07
**PoC by**: claude-opus-4.6, high

### Changes Made

- **`cmd/stellar-rpc/internal/db/ledger.go:35-52`** — Added `StreamLedgerRange` and `StreamLedgersBySequences` methods to the `LedgerReaderTx` interface, closing the API gap identified in the hypothesis.

- **`cmd/stellar-rpc/internal/db/ledger.go:215-270`** — Implemented both streaming methods on `ledgerReaderTx`. Each uses `l.tx.Query(ctx, sql)` for row-by-row iteration with `Scan` into `xdr.LedgerCloseMeta`, following the proven pattern from `ledgerReader.StreamLedgerRange`. The callback `f` is invoked per row; returning a non-nil error stops iteration immediately.

- **`cmd/stellar-rpc/internal/methods/get_transactions.go:38`** — Added `errPageFull` sentinel error used by the streaming callback to signal page completion without indicating a real failure.

- **`cmd/stellar-rpc/internal/methods/get_transactions.go:436-475`** — Refactored the XDR (non-JSON, non-cache) path in `getTransactionsByLedgerSequence` to use `readTx.StreamLedgersBySequences` instead of `BatchGetLedgersBySequences` + materialization loop. The streaming callback calls `processTransactionsInLedger` per row and returns `errPageFull` when the page is full, stopping the SQL cursor early. The JSON path retains batch fetch since it needs raw LCM bytes for the Rust FFI.

- **`cmd/stellar-rpc/internal/methods/mocks.go:103-116`** — Added `StreamLedgerRange` and `StreamLedgersBySequences` mock methods to `MockLedgerReaderTx` to satisfy the updated interface.

### Demonstration

The optimization adds tx-scoped streaming primitives (`StreamLedgerRange`, `StreamLedgersBySequences`) to `LedgerReaderTx`, using `Query` for row-by-row iteration instead of `Select`-based full materialization. The XDR path in `getTransactions` now processes each ledger as it arrives from SQLite and stops the cursor immediately when the page is full, eliminating unnecessary XDR deserialization of remaining rows. For dense-ledger, small-limit requests (e.g., limit=1 on a ledger with 50 transactions), this avoids deserializing all fetched ledgers when only the first is needed.

### Test Results

All Go tests pass: `db` (0.5s), `methods` (1.5s), and the full `go test ./...` suite (all packages OK). No test failures or regressions.

---

## Final Review

**Verdict**: REJECTED
**Date**: 2026-04-07
**Final review by**: gpt-5.4, high
**Failed At**: final-review

### Adversarial Analysis

1. **Does the change address the claimed inefficiency?** Partially in theory, but not on the actual benchmarked path. The current `getTransactions` implementation already narrows work to ledgers returned by `GetLedgerSequencesWithTransactions`, and the reviewed change only swaps the non-JSON/XDR branch from `BatchGetLedgersBySequences` to `StreamLedgersBySequences`.
2. **Are the preconditions realistic?** Not for the project's benchmark workload. `stellar-rpc-blaster` sends a 50/50 JSON/base64 mix for `getTransactions`, so this change can only affect about half of requests even before considering that the index-driven ledger selection has already removed most empty-ledger overfetch.
3. **Is the original code inefficient or working as designed?** The old path still had some theoretical waste, but the remaining waste is too small to dominate end-to-end latency after the earlier index-based pruning.
4. **Does the benchmark improvement match the claim?** No. Independent reruns on isolated baseline/optimized trees using the project blaster, the same seeded data, and the same synced futurenet DB showed a regression in p50 latency at every tested load and no throughput gain:
   - **100 RPS**: p50 `6.063 -> 6.655 ms` (**9.76% worse**), p95 `32.239 -> 32.863 ms` (**1.94% worse**), p99 `35.903 -> 36.063 ms` (**0.45% worse**)
   - **200 RPS**: p50 `6.307 -> 6.875 ms` (**9.01% worse**), p95 `34.079 -> 33.663 ms` (**1.22% better**), p99 `38.239 -> 38.463 ms` (**0.59% worse**)
   - **200 RPS warm baseline recheck**: p50 `6.247 -> 6.875 ms` (**10.05% worse**), p95 `33.663 -> 33.663 ms` (**flat**), p99 `37.983 -> 38.463 ms` (**1.26% worse**)
   - **300 RPS**: p50 `6.847 -> 7.343 ms` (**7.24% worse**), p95 `35.135 -> 35.487 ms` (**1.00% worse**), p99 `41.823 -> 41.183 ms` (**1.53% better**)
   - **Throughput ceiling in the tested range**: `300 RPS -> 300 RPS` with zero errors for both builds
5. **Is the optimization in scope?** Yes, but it does not produce the claimed performance benefit.
6. **Is the benchmark methodology correct?** Yes. This review used `make -j8 build-stellar-rpc`, `make go-test`, `cargo test`, `stellar-rpc-blaster generate`, and a real blaster sweep on a local futurenet-backed RPC instance.
7. **Can the observed result be explained without the optimization?** Yes. The added row-by-row streaming/callback path likely adds overhead while only touching the XDR half of mixed benchmark traffic, so the theoretical savings are outweighed by the unchanged JSON half and the already-pruned selected-ledger fetch path.
8. **Is this optimization novel?** Irrelevant to the verdict; measured performance regressed.

### Rejection Reason

Independent benchmarking does not confirm the hypothesis. The reviewed change makes `getTransactions` slower on median latency at 100, 200, and 300 RPS and does not increase zero-error throughput, so the optimization claim is unsupported.

### Failed Checks

1, 2, 4, 7
