# H005: Duplicate diagnostic-event lookups do not remove the expensive work

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Low
**Impact**: CPU overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

`ParseTransaction()` should extract diagnostic events once per transaction and reuse that result when building the response fields. If the same event set is already available from a broader helper, the parser should not re-fetch it on the hot path.

## Mechanism

I suspected `ParseTransaction()` was redundantly doing event work because it calls `ingestTx.GetTransactionEvents()` and then separately calls `ingestTx.GetDiagnosticEvents()`, even though the SDK’s `TransactionEvents` struct already includes `DiagnosticEvents`. If that implied duplicate event extraction or duplicate event marshaling, removing the second lookup could have reduced work for Soroban-heavy pages.

## Trigger

Use `getTransactions` on ledgers containing Soroban transactions with diagnostic events and trace the event-extraction path through `ParseTransaction()` and the SDK’s ledger transaction helpers.

## Target Code

- `cmd/stellar-rpc/internal/db/transaction.go:ParseTransaction:268-287` — calls both `GetTransactionEvents()` and `GetDiagnosticEvents()`
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/ingest/ledger_transaction.go:GetDiagnosticEvents:264-265` — getter is just a thin pass-through
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/ingest/ledger_transaction.go:GetTransactionEvents:278-307` — populates `DiagnosticEvents` alongside other event slices

## Evidence

`ParseTransaction()` clearly performs both lookups (`cmd/stellar-rpc/internal/db/transaction.go:268-287`). In the SDK, `GetTransactionEvents()` does populate `TransactionEvents.DiagnosticEvents` (`/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/ingest/ledger_transaction.go:278-307`), so there is a superficially redundant second call to `GetDiagnosticEvents()` (`/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/ingest/ledger_transaction.go:264-265`).

## Anti-Evidence

The duplicate lookup is only at the metadata-access level. The expensive part — marshalling each diagnostic event into `[]byte` in `ParseTransaction()` — still happens exactly once, and the SDK getters mostly return existing slices or build only a shallow outer slice for operation events.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The apparent duplication does not duplicate the expensive `MarshalBinary()` loops, so eliminating the second getter call would only save a tiny amount of metadata plumbing.

### Lesson Learned

When event handling looks duplicated, distinguish between cheap slice/header assembly and the actual byte serialization. Only the latter is large enough to matter for `getTransactions`.
