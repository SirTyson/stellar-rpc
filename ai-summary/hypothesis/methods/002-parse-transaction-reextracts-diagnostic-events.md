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
