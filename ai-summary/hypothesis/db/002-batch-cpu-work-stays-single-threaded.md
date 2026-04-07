# H002: Batch-fetched ledgers are still parsed and encoded on one goroutine

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Medium
**Impact**: CPU latency / multicore underutilization
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

After a `getTransactions` batch has already been materialized into Go memory, CPU-heavy transaction parsing and response encoding should use available cores when the page spans multiple ledgers or many transactions. Large pages should not remain bottlenecked on one goroutine once the SQL read has completed.

## Mechanism

`BatchGetLedgerMetas()` eagerly returns a fully materialized `[]xdr.LedgerCloseMeta`, and the subsequent work in `processTransactionsInLedger()` is pure in-memory CPU: transaction reader setup, per-transaction marshaling, base64 encoding, and optional Rust JSON conversion. The handler currently processes that whole batch sequentially, so dense multi-ledger pages leave cores idle even though the batch prefix needed to satisfy the limit can be computed from `CountTransactions()` and processed independently before the final ordered append.

## Trigger

Request a large page (`limit` near the configured max) over several dense ledgers on a multicore machine, especially in `json` format. A CPU profile should show one request goroutine spending most of its time in `processTransactionsInLedger()`, `db.ParseTransaction()`, `transactionToJSON()`, and event conversion while other cores remain underused.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:314-367` — the outer loop fetches a full batch, then processes each ledger strictly serially
- `cmd/stellar-rpc/internal/db/ledger.go:BatchGetLedgerMetas:118-140` — all ledgers for the batch are already materialized before CPU work begins
- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:76-233` — CPU-only per-ledger parsing and encoding stage

## Evidence

`getTransactionsByLedgerSequence()` fetches `ledgers` first and only then iterates them one by one (`cmd/stellar-rpc/internal/methods/get_transactions.go:330-367`). `BatchGetLedgerMetas()` has already completed the SQL work and returned a Go slice before that loop starts (`cmd/stellar-rpc/internal/db/ledger.go:120-139`). From there on, `processTransactionsInLedger()` only touches in-memory data structures plus local base64/FFI conversion (`cmd/stellar-rpc/internal/methods/get_transactions.go:88-233`), which means the hot stage is parallelizable once the minimal ledger prefix needed for the page is known.

## Anti-Evidence

Small pages that finish in one ledger will not benefit and may regress if goroutine scheduling overhead is added blindly. Any fix has to preserve response ordering, early-stop semantics, and error attribution, so it likely needs to parallelize only the batch prefix that is definitely required.
