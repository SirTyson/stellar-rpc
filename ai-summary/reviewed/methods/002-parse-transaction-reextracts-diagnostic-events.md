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
