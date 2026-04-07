# H008: Event helper slice-header rebuilding on the XDR path is too small to matter

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Low
**Impact**: CPU / allocation overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

The default `xdr` path should avoid per-transaction helper allocations if they meaningfully affect `getTransactions` latency. If event extraction helpers are hot enough, the handler should not rebuild temporary event containers before base64 encoding them.

## Mechanism

I suspected the XDR branch was paying avoidable overhead because it calls `GetDiagnosticEvents()` and `GetTransactionEvents()` for every transaction before base64-encoding the events. If those helpers were cloning large event payloads or doing deep per-event work, bypassing them could have reduced the steady-state cost of event-heavy pages.

## Trigger

Call `getTransactions` in default `xdr` format over Soroban-heavy ledgers with many emitted events, then inspect the per-transaction event-extraction path before the actual `MarshalBase64()` loops run.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:199-216` — XDR branch calls `GetDiagnosticEvents()` and `GetTransactionEvents()` for every returned transaction
- `cmd/stellar-rpc/internal/methods/get_transactions.go:238-264` — actual event encoding happens afterward in `buildEventsXDRDirect`
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/ingest/ledger_transaction.go:GetDiagnosticEvents:260-265` — diagnostic getter delegates directly to `UnsafeMeta`
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/ingest/ledger_transaction.go:GetTransactionEvents:278-307` — V4 path mostly assigns existing slices and allocates only the outer `OperationEvents` container

## Evidence

The local handler clearly invokes both helper getters before encoding events (`cmd/stellar-rpc/internal/methods/get_transactions.go:199-216`). In the SDK, `GetDiagnosticEvents()` is just a thin pass-through (`/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/ingest/ledger_transaction.go:264-265`), and `GetTransactionEvents()` for V4 mainly assigns existing `txMeta.Events`, `txMeta.DiagnosticEvents`, and `op.Events`, allocating only a fresh outer slice for `OperationEvents` (`/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/ingest/ledger_transaction.go:300-307`).

## Anti-Evidence

The expensive work on this path is still the subsequent `MarshalBase64()` over each actual event payload in `buildEventsXDRDirect()`, not the slice-header assembly in the SDK helpers (`cmd/stellar-rpc/internal/methods/get_transactions.go:238-264`). Eliminating the helper allocations would save only a tiny constant compared with the unavoidable per-event serialization cost.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The helper functions mostly reuse existing event slices, so the suspected waste is limited to small outer-slice/header allocations and does not remove the dominant base64 serialization work.

### Lesson Learned

For `getTransactions`, event-path optimizations need to eliminate actual per-event marshaling or repeated cross-request work. Pure slice-shape cleanup inside SDK helpers is too far below the dominant cost to matter.
