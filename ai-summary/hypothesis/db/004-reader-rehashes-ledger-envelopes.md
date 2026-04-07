# H004: Per-request ledger readers rebuild full envelope hash tables for ledgers they only page through once

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Medium
**Impact**: CPU / hashing / allocation churn
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Repeated `getTransactions` calls over the same hot ledgers should avoid redoing full per-ledger envelope flattening and transaction hashing when only a small page is being returned. Setup work should be closer to the number of transactions actually consumed by the page, or should be reusable across requests for the same retained ledger.

## Mechanism

For every scanned ledger, `processTransactionsInLedger()` creates a new `ingest.NewLedgerTransactionReaderFromLedgerCloseMeta()`. That constructor allocates a `map[xdr.Hash]xdr.TransactionEnvelope` sized to `CountTransactions()`, flattens the ledger's transaction set into a fresh slice, and hashes every envelope before the first `Read()`. On polling-heavy workloads that revisit the same recent ledgers with low limits, the endpoint keeps rebuilding identical lookup tables, so CPU and allocation cost scales with total transactions in each ledger instead of the page size.

## Trigger

Use a ledger window with very dense latest ledgers, then repeatedly poll `getTransactions` with a cursor near the tip and `limit=10`. A profile should show repeated time in transaction hashing and map allocation even when each request only returns a small prefix of the same ledgers.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:85-115` — constructs a fresh ledger transaction reader per ledger
- `cmd/stellar-rpc/internal/db/transaction.go:getTransactionByHash:221-235` — same reader construction pattern on the single-transaction path, showing no local reuse today

## Evidence

The local code creates a brand-new ingest reader for each ledger scan (`cmd/stellar-rpc/internal/methods/get_transactions.go:85-115`). In the SDK code that reader immediately allocates `envelopesByHash`, calls `storeTransactions()`, and hashes every envelope in `TransactionEnvelopes()` before any transaction is read (`/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/ingest/ledger_transaction_reader.go:48-60,123-149`); `TransactionEnvelopes()` itself also allocates a fresh slice of all envelopes (`/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/xdr/ledger_close_meta.go:62-93`).

## Anti-Evidence

If a request consumes most transactions from each ledger exactly once, the setup cost is amortized across a larger useful payload. Any fix must preserve the SDK's envelope-to-result matching semantics, because the tx-set order does not match processing order.
