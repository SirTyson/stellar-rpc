# H003: ledgerTransactionReader Duplicates Full Envelopes Before the First Transaction Read

**Date**: 2026-04-07
**Subsystem**: daemon
**Severity**: Medium
**Impact**: hot-ledger allocation and copy churn
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Building the per-ledger reader for `getTransactions` should hash each envelope once without materializing and then recopying the full `TransactionEnvelope` set before any transaction is returned. The hot path should retain at most one in-memory representation of each envelope while it builds the application-order lookup.

## Mechanism

`xdr.LedgerCloseMeta.TransactionEnvelopes()` already allocates a fresh `[]TransactionEnvelope` and appends every envelope from the phase structure into that slice. `storeTransactions` then ranges that slice and stores each `TransactionEnvelope` again as the value in `envelopesByHash`, so dense ledgers pay two whole-envelope materializations before `Read()` serves even the first transaction. A local phase iterator plus a hash-to-index map (or another indirection that avoids copying the full envelope value twice) would remove this extra allocation/copy surface from every `getTransactions` request.

## Trigger

Profile `getTransactions` on ledgers with many transactions or large envelopes, especially small-limit pages that still have to build the reader for the starting ledger. Compare the current path to a version that walks phases directly and stores lightweight indices instead of a second full-envelope copy.

## Target Code

- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/xdr/ledger_close_meta.go:62-94` — `TransactionEnvelopes()` allocates a fresh flattened slice and appends every envelope into it.
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:20-41` — reader stores a `map[xdr.Hash]xdr.TransactionEnvelope`, not an index or pointer.
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:84-104` — `storeTransactions` iterates the flattened slice and copies each envelope into the map value.
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:44-73` — `Read()` only needs the envelope corresponding to the requested hash, so a lighter indirection is sufficient.

## Evidence

The SDK helper returns a freshly allocated envelope slice with capacity `CountTransactions()`, which means the flattened envelope set already exists as a complete collection before hashing begins. The daemon reader immediately copies each of those envelope values into `envelopesByHash`, so the same logical payload is stored twice per ledger in the hot path.

## Anti-Evidence

The envelope hash computation itself is still required, so this does not remove the SHA-256/XDR hashing cost already known on this path. The win is also workload-dependent: classical ledgers with small envelopes benefit less than ledgers with many operations or large fee-bump envelopes.
