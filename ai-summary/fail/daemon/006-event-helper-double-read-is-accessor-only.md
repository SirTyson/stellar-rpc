# H006: Event Helper Double-Read Adds a Meaningful getTransactions Hot-Path Cost

**Date**: 2026-04-07
**Subsystem**: daemon
**Severity**: Low
**Impact**: event extraction overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

If the XDR path asks for diagnostic events and transaction/contract events separately, those helper calls should not duplicate any expensive traversal of transaction metadata. The expensive part of the endpoint should remain the actual XDR/base64 or JSON serialization, not lightweight accessors over already-decoded metadata.

## Mechanism

I suspected the XDR path in `processTransactionsInLedger` might walk the same transaction meta twice because it calls `ingestTx.GetDiagnosticEvents()` and `ingestTx.GetTransactionEvents()` separately. If both helpers re-traversed or rebuilt the same event structures, every transaction would pay duplicated pre-serialization CPU before any `MarshalBase64` or `xdr2json` work began.

## Trigger

Trace the helper implementations used by the XDR branch and compare them with profiles from event-heavy `getTransactions` pages.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:200-219` — XDR path calls `GetDiagnosticEvents()` and `GetTransactionEvents()` back-to-back.
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/ingest/ledger_transaction.go:264-312` — helper implementations.
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/xdr/transaction_meta.go:31-63` — underlying `TransactionMeta` accessors.

## Evidence

The daemon does make two separate helper calls on the same `ingestTx` before encoding events, so there is at least a plausible duplicated-work angle at the call site.

## Anti-Evidence

`LedgerTransaction.GetDiagnosticEvents()` is just `return t.UnsafeMeta.GetDiagnosticEvents()`, and the underlying `TransactionMeta` helpers are simple version-switch accessors that return existing slices. Even where `GetTransactionEvents()` assembles an event grouping, the expensive work on the hot path is still the later `MarshalBase64`/JSON conversion, not these accessor calls.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The suspected duplicated extraction is mostly slice access and light grouping logic, not a second full metadata traversal with meaningful CPU cost. Removing these calls would not materially change `getTransactions` latency next to the actual event serialization work.

### Lesson Learned

For event-heavy `getTransactions` analysis, distinguish accessors from serializers. The hot costs are the per-event `MarshalBase64` / JSON conversion steps, not the helper methods that expose already-decoded event slices.
