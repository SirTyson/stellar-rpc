# H007: Per-request read-only transaction setup is too small to explain endpoint cost

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Low
**Impact**: DB setup overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

`getTransactions` should avoid unnecessary request-scoped DB setup on the hot path. If session cloning or opening the read-only SQLite transaction were a meaningful fraction of request time, that setup should be reduced or amortized.

## Mechanism

I suspected `ledgerReader.NewTx()` might be an unexpectedly expensive fixed overhead because every request clones the DB session and begins a read-only transaction before doing any useful work. If those setup calls were large enough, eliminating the explicit transaction or reusing a session across requests could improve small-page latency.

## Trigger

Issue many tiny `getTransactions` requests against a warm DB and inspect the fixed per-request cost before ledger-range lookup and batch reads begin.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:206-215` — every request opens a read transaction up front
- `cmd/stellar-rpc/internal/db/ledger.go:ledgerReader.NewTx:155-168` — cloned session + `BeginTx(..., ReadOnly: true)`
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/support/db/session.go:61-105` — `BeginTx()` and `Clone()` implementation details

## Evidence

`getTransactionsByLedgerSequence()` always calls `h.ledgerReader.NewTx(ctx)` before validation or paging (`cmd/stellar-rpc/internal/methods/get_transactions.go:206-215`). In the SDK session layer, `Clone()` is just a shallow wrapper that reuses the underlying `DB` handle, while `BeginTx()` is one `sqlx.BeginTxx` call on that shared handle (`/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/support/db/session.go:61-105`).

## Anti-Evidence

This setup cost is dwarfed by the rest of the request: range lookup, `BatchGetLedgerMetas()` blob reads, full `LedgerCloseMeta` unmarshaling, SDK reader construction, `ParseTransaction()` marshaling, and optional Rust JSON conversion. The read transaction also provides snapshot consistency across multi-batch scans, so simply removing it would trade correctness for a very small fixed-cost saving.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

`Clone()` is effectively free and `BeginTx()` is only a single DB call, so even perfect elimination would not remove enough work to move `getTransactions` latency meaningfully.

### Lesson Learned

Fixed request setup is only worth tracking on this endpoint when it dominates the steady-state path. Here, the real costs are blob reads, XDR processing, and conversion work after the transaction is already open.
