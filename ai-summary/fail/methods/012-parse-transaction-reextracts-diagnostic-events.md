# H002: ParseTransaction Extracts Diagnostic Events Twice for Soroban Transactions

**Date**: 2026-04-06
**Subsystem**: methods
**Severity**: Low
**Impact**: CPU / allocations
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

For a transaction that carries diagnostic events, `getTransactions` should extract those events once and reuse the result for both the `diagnosticEvents*` fields and the `events` object. Event-heavy Soroban pages should not walk the same metadata twice just to re-materialize the same diagnostic event slice.

## Mechanism

`db.ParseTransaction` first calls `ingestTx.GetTransactionEvents()`, then separately calls `ingestTx.GetDiagnosticEvents()`, and finally ignores `allEvents.DiagnosticEvents`. In the SDK, `GetTransactionEvents()` already populates diagnostic events for V3 and V4 transaction metadata, including an internal `GetDiagnosticEvents()` call on V3. As a result, Soroban transactions can pay duplicate diagnostic-event extraction before the handler even reaches JSON or XDR serialization.

## Trigger

1. Use `getTransactions` on ledgers containing Soroban transactions with large diagnostic-event payloads.
2. Compare CPU samples or allocation counts for the current code versus a variant that reuses `allEvents.DiagnosticEvents` instead of calling `GetDiagnosticEvents()` a second time.
3. Focus especially on V3 metadata or ledgers produced with diagnostic events enabled, where duplicate extraction should be most visible.

## Target Code

- `cmd/stellar-rpc/internal/db/transaction.go:ParseTransaction:268-289` — `GetTransactionEvents()` and `GetDiagnosticEvents()` are both called, but only the latter is consumed for `tx.Events`.
- `cmd/stellar-rpc/internal/db/transaction.go:parseEvents:295-320` — `allEvents.DiagnosticEvents` is ignored.
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/ingest/ledger_transaction.go:GetDiagnosticEvents:264-266` — direct diagnostic-event helper.
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/ingest/ledger_transaction.go:GetTransactionEvents:278-311` — event extraction already carries diagnostic events in the returned struct.

## Evidence

The local code asks for transaction events and diagnostic events as two separate operations, then only marshals the latter into `tx.Events`. The SDK implementation shows that `TransactionEvents` already contains a `DiagnosticEvents` field, and the V3 branch explicitly calls `GetDiagnosticEvents()` while building that struct, so the second call in `ParseTransaction` is redundant on the hot Soroban path.

## Anti-Evidence

Classic transactions and older metadata versions return empty event sets quickly, so the savings concentrate on Soroban-heavy workloads. On V4 metadata, `GetDiagnosticEvents()` is just a shallow accessor to `txMeta.DiagnosticEvents`, so the win is more about avoiding duplicate helper calls and slice plumbing than eliminating a large parse.

---

## Review

**Verdict**: VIABLE
**Severity**: Informational
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full call chain from `ParseTransaction` (transaction.go:268-289) through both SDK methods. `GetTransactionEvents()` (ledger_transaction.go:278-311) already populates its `DiagnosticEvents` field for both V3 (via internal `GetDiagnosticEvents()` call at line 293) and V4 (via direct field copy at line 303). The separate `GetDiagnosticEvents()` call at transaction.go:273 redundantly accesses the same struct fields. However, both paths are O(1) struct field accesses on already-deserialized XDR data — there is no re-parsing, no allocation, and no meaningful CPU cost.

### Code Paths Examined

- `cmd/stellar-rpc/internal/db/transaction.go:ParseTransaction:268-289` — confirmed: calls `GetTransactionEvents()` then separately calls `GetDiagnosticEvents()`, uses only the latter for `tx.Events`, ignores `allEvents.DiagnosticEvents`
- `go-stellar-sdk@v0.4.0/ingest/ledger_transaction.go:GetTransactionEvents:278-311` — V3: calls `GetDiagnosticEvents()` internally to populate `txEvents.DiagnosticEvents`; V4: copies `txMeta.DiagnosticEvents` directly
- `go-stellar-sdk@v0.4.0/ingest/ledger_transaction.go:GetDiagnosticEvents:264-266` — delegates to `UnsafeMeta.GetDiagnosticEvents()`, which is a switch + field access (xdr/transaction_meta.go:35-50)
- `go-stellar-sdk@v0.4.0/xdr/transaction_meta.go:GetDiagnosticEvents:35-50` — V3: reads `sorobanMeta.DiagnosticEvents` field; V4: reads `MustV4().DiagnosticEvents` field; both are direct struct field accesses with zero allocation

### Findings

The redundancy is real: `allEvents.DiagnosticEvents` already contains the exact same slice that `GetDiagnosticEvents()` returns (they reference the same underlying XDR struct fields). The fix — replacing the separate `GetDiagnosticEvents()` call with `allEvents.DiagnosticEvents` — is correct and safe for all metadata versions (V1/V2: both nil, V3: same sorobanMeta.DiagnosticEvents, V4: same txMeta.DiagnosticEvents).

However, the hypothesis significantly overstates the impact. The term "extraction" implies re-parsing or significant work, but both calls are trivial O(1) struct field accesses with zero allocations. The redundant call costs ~5-10 nanoseconds per transaction. Compare this to the `MarshalBinary()` calls in the same function (lines 258-266 for Result/Meta/Envelope, lines 280-284 for each diagnostic event) which each perform actual serialization work costing 100ns-10μs+. The redundant call represents well under 0.001% of `ParseTransaction`'s total cost.

**Severity downgrade rationale**: Original claim was "Low" (<5% measurable improvement). Actual impact is unmeasurable — a single struct field access redundancy amidst heavy serialization work. Downgraded to Informational. This is a valid code cleanup but not a performance optimization.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/db/transaction.go:ParseTransaction` lines 273-285
- **Change description**: Remove the separate `GetDiagnosticEvents()` call (line 273) and replace `diagEvents` with `allEvents.DiagnosticEvents` on line 278. The `parseEvents` function can also be extended to handle diagnostic events, consolidating all event marshaling in one place.
- **Correctness check**: Existing tests for `ParseTransaction` and `getTransactions` handler should pass unchanged. Verify that `tx.Events` output is identical before/after.
- **Benchmark focus**: No measurable improvement expected. A micro-benchmark of `ParseTransaction` with Soroban transactions would show noise-level differences at best. This is better validated as a code-correctness/cleanup change than a performance optimization.

---

## PoC Attempt

**Result**: POC_PASS
**Date**: 2026-04-07
**PoC by**: claude-opus-4.6, high

### Changes Made

- `cmd/stellar-rpc/internal/db/transaction.go` (lines 268-316): Removed the redundant `ingestTx.GetDiagnosticEvents()` call and the inline diagnostic event marshaling loop from `ParseTransaction`. Moved diagnostic event marshaling into `parseEvents`, which now uses `allEvents.DiagnosticEvents` (already populated by `GetTransactionEvents()`) to populate `tx.Events`. This consolidates all event marshaling (diagnostic, transaction, and contract events) into a single function.

### Demonstration

The optimization eliminates a redundant SDK call by reusing the `DiagnosticEvents` field already present in the `TransactionEvents` struct returned by `GetTransactionEvents()`. While the actual performance impact is negligible (O(1) struct field access), it improves code clarity by consolidating all event marshaling into `parseEvents` and removing dead data flow where `allEvents.DiagnosticEvents` was previously ignored.

### Test Results

All Go tests pass: `db` (0.481s), `methods` (0.276s), `ingest` (0.036s), `integrationtest` (0.078s), and all other packages. All Rust tests pass (2 tests). Build succeeds cleanly.

---

## Final Review — Needs Revision

**Date**: 2026-04-07
**Final review by**: gpt-5.4, high

### What Needs Fixing

The code change is real and safe, but the performance evidence is not controlled tightly enough to confirm the finding. I isolated H002 in a clean worktree based on commit `651743f`, verified `make -j8 build-stellar-rpc` and `make go-test` pass before and after the one-file `transaction.go` change, and then benchmarked with `stellar-rpc-blaster`.

The first A/B sweep was not apples-to-apples:

- The optimized run used a freshly generated seed corpus immediately before benchmarking, which warmed the RPC data path.
- The baseline run initially reused that seed file without the same warm-up and was much colder.
- Results were unstable across load levels: 100 RPS was nearly identical, 200 RPS showed a large gap, 300 RPS was slightly worse for the optimized build, and both variants degraded badly by 400 RPS.

Independent measurements:

| Run | p50 | p95 | p99 | Errors |
|---|---:|---:|---:|---:|
| Baseline 100 RPS | 17.567 ms | 48.191 ms | 54.655 ms | 0 |
| Optimized 100 RPS | 17.119 ms | 47.935 ms | 54.399 ms | 0 |
| Baseline 200 RPS (cold) | 235.391 ms | 452.863 ms | 532.991 ms | 0 |
| Optimized 200 RPS | 62.623 ms | 190.591 ms | 243.583 ms | 0 |
| Baseline 300 RPS | 3489.791 ms | 8343.551 ms | 10076.159 ms | 0 |
| Optimized 300 RPS | 3688.447 ms | 8314.879 ms | 9543.679 ms | 0 |
| Baseline 400 RPS | 3551.231 ms | 15007.743 ms | 15007.743 ms | 1702 |
| Optimized 400 RPS | 3475.455 ms | 12279.807 ms | 15007.743 ms | 1449 |

I then reran the clean baseline with the same `generate` warm-up step immediately before a 200 RPS run. That alone dropped baseline p50 from `235.391 ms` to `125.2 ms`, showing the original 200 RPS delta was strongly influenced by cache/warm-up effects. That still does not prove H002 has zero impact, but it does mean the current PoC does not isolate H002 well enough to confirm it.

### Revision Instructions

1. Benchmark **only** the isolated H002 change in a clean worktree. Do not include other `getTransactions`, `jsonrpc`, `xdr2json`, or reader changes in the benchmarked build.
2. Use the **same seed corpus and same warm-up procedure** for baseline and optimized runs. Either run `generate` immediately before both variants, or before neither.
3. Repeat the comparison at least **A/B/A/B** at the same RPS (200 RPS is the interesting point from this review) to rule out warm-cache and one-off system variance.
4. Keep the final judgment tied to `getTransactions` only. If the effect remains negligible or inconsistent after controlled paired runs, downgrade this to a cleanup / informational change rather than a confirmed performance optimization.
5. If you want to argue that event-heavy Soroban pages amplify the win, show that with a controlled seed corpus containing those transactions rather than relying on mixed futurenet traffic alone.

### Checks Passed So Far

- The redundancy claim in `ParseTransaction` is correct: `GetTransactionEvents()` already carries diagnostic events, and reusing `allEvents.DiagnosticEvents` is behavior-safe.
- The isolated H002 patch builds cleanly and passes the existing Go test suite.
- The independent blaster runs show the optimization is **not** obviously harmful, but they do **not** yet establish a stable, attributable throughput or latency win.

---

## Final Review

**Verdict**: REJECTED
**Date**: 2026-04-07
**Final review by**: gpt-5.4, high
**Failed At**: final-review

### Adversarial Analysis

1. **Does the change actually address the claimed inefficiency?** **YES, but only as cleanup.** On the code path, `ParseTransaction` no longer calls `ingestTx.GetDiagnosticEvents()` separately and instead reuses `allEvents.DiagnosticEvents` from `GetTransactionEvents()`. That is behavior-safe, but it only removes a trivial redundant accessor.
2. **Are the preconditions realistic?** **YES.** Soroban-heavy `getTransactions` pages with diagnostic events are realistic, and the benchmark used real futurenet-backed request data generated by `stellar-rpc-blaster`.
3. **Is the original code inefficient or working as designed?** **MINOR INEFFICIENCY.** The redundant helper call is real, but the SDK implementation shows it is just struct-field access / slice reuse, not expensive re-parsing.
4. **Does the benchmark improvement match the claimed severity?** **NO.** On the valid same-base A/B run, the optimization is slightly slower at the stable load level and much worse once load increases. The valid throughput ceiling remains **100 RPS** for both variants because p50 latency jumps by far more than 20% when moving from 100 to 200 RPS.

   | Run | p50 ms | p95 ms | p99 ms | Errors |
   |---|---:|---:|---:|---:|
   | Baseline 100 RPS | 17.215 | 47.487 | 55.327 | 0 |
   | Optimized 100 RPS | 18.623 | 48.415 | 55.647 | 0 |
   | Baseline 200 RPS | 112.127 | 355.327 | 464.383 | 0 |
   | Optimized 200 RPS | 350.207 | 892.927 | 1048.063 | 0 |
   | Baseline 200 RPS recheck | 71.999 | 206.207 | 258.175 | 0 |
   | Optimized 200 RPS recheck | 221.055 | 605.183 | 721.407 | 0 |
   | Baseline 300 RPS | 3409.919 | 8691.711 | 9076.735 | 0 |
   | Optimized 300 RPS | 3311.615 | 8560.639 | 9117.695 | 0 |

   At 100 RPS, the H002 patch is **slower**: p50 +8.18%, p95 +1.95%, p99 +0.58%. At 200 RPS it is dramatically worse on both the first run and the recheck.
5. **Is the optimization in scope?** **YES.** The change is confined to `cmd/stellar-rpc/internal/db/transaction.go`, which is in the `getTransactions` call chain.
6. **Is the benchmark methodology correct?** **YES.** I benchmarked the true baseline commit (`08797b0`) and then benchmarked that exact same commit with only the isolated H002 patch applied in `transaction.go`. Both variants used the same futurenet-backed DB, the same generated seed corpus, the same warm-up pattern, and the same blaster config and RPS sweep.
7. **Can the improvement be explained WITHOUT the optimization?** **YES.** There is no improvement to explain. The only repeatable signal from the same-base comparison is that H002 does not improve end-to-end `getTransactions` performance and can be slower under load.
8. **Is this optimization novel?** **IRRELEVANT TO VERDICT.** Even if novel, the measured result does not support it as a performance finding.

### Rejection Reason

The code cleanup is real, but it does not produce a measurable `getTransactions` performance win. In the valid same-base benchmark, throughput ceiling stays at 100 RPS and the patched build is slightly slower at 100 RPS and substantially slower at 200 RPS.

### Failed Checks

- 4
- 7
