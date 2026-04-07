# H002: By-Sequence Ledger Fetch Keeps Raw Blobs and Re-Decodes Full `LedgerCloseMeta` Values in the Handler

**Date**: 2026-04-07
**Subsystem**: methods
**Severity**: Medium
**Impact**: latency / allocations / GC pressure
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once `getTransactions` has already selected the exact ledger sequences it needs, each fetched row should be XDR-decoded once and carried through the hot path as a typed `xdr.LedgerCloseMeta`. Presence validation should use the SQL `sequence` column directly rather than reparsing raw blobs and then asking the handler to unmarshal those same blobs again.

## Mechanism

`BatchGetLedgersBySequences()` currently scans `meta` into `[][]byte`, partially parses each blob just to recover the ledger sequence, and then `getTransactionsByLedgerSequence()` calls `lcm.UnmarshalBinary(chunk.Lcm)` before transaction processing. That leaves every selected ledger paying for a SQLite blob copy, temporary raw-blob retention, header parsing in the DB helper, and a second full `LedgerCloseMeta` decode in the handler. The codebase already proves a cheaper pattern is available: `getTransactionByHash()` scans `meta` directly into a struct field of type `xdr.LedgerCloseMeta`, so a `BatchGetLedgerMetasBySequences()` helper that selects `sequence, meta` should be able to remove the raw-blob handoff and the second full decode.

## Trigger

1. Use `getTransactions` on a sparse window where the planner correctly selects many non-empty ledgers that are all actually needed to fill the page.
2. Profile allocations and CPU in the `BatchGetLedgersBySequences` -> `UnmarshalBinary` path.
3. Compare against a version that scans `SELECT sequence, meta` directly into typed `xdr.LedgerCloseMeta` rows and validates presence from the SQL sequence column.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:315-349` — the handler receives raw `chunk.Lcm` blobs and fully unmarshals each ledger before processing.
- `cmd/stellar-rpc/internal/db/ledger.go:162-204` — `BatchGetLedgersBySequences()` materializes `[][]byte` and partially reparses each blob.
- `cmd/stellar-rpc/internal/db/transaction.go:227-245` — `getTransactionByHash()` is existing prior art for scanning `meta` directly into `xdr.LedgerCloseMeta`.

## Evidence

The by-sequence helper is unique in forcing the handler to own the full `UnmarshalBinary()` step after the DB layer has already touched the same blob. In contrast, the existing single-transaction lookup shows that `support/db.Select` can populate typed XDR fields directly inside Go structs. That makes the current raw-blob handoff look like an avoidable compatibility artifact rather than a hard requirement of the DB stack.

## Anti-Evidence

This does not reduce how many ledgers the planner selects, so dense-page overfetch remains the larger problem when selection is too broad. The win therefore concentrates on workloads where the selected ledgers are genuinely needed and the repeated decode/copy overhead is a meaningful fraction of total request time.
