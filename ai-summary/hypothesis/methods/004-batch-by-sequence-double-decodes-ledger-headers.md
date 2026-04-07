# H004: `BatchGetLedgersBySequences` Decodes Every Selected Ledger Header Twice

**Date**: 2026-04-07
**Subsystem**: methods
**Severity**: Low
**Impact**: CPU / allocations
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When `getTransactions` fetches a non-contiguous set of ledgers, the DB helper should return the ledger sequence number directly from SQL and let the caller decode each `LedgerCloseMeta` exactly once. It should not partially unmarshal every blob just to recover a sequence number that is already stored in the `sequence` column.

## Mechanism

`BatchGetLedgersBySequences()` selects only `meta`, then partially XDR-decodes version, extension, and header for every row so it can populate `chunk.Header.Header.LedgerSeq` for the caller's completeness check. The handler later fully unmarshals `chunk.Lcm` before transaction processing. Changing the query to return `sequence, meta` would let the missing-ledger verification use the SQL column directly, removing the extra `bytes.Reader` creation and XDR decode on every fetched ledger.

## Trigger

1. Use a request that selects many non-empty ledgers through the transaction index (for example, `limit=200` on a sparse window with one transaction per ledger).
2. Profile CPU samples and allocations in the current indexed path.
3. Compare against a version of `BatchGetLedgersBySequences()` that selects `sequence` alongside `meta` and skips the partial header parse.

## Target Code

- `cmd/stellar-rpc/internal/db/ledger.go:BatchGetLedgersBySequences:164-204` — fetches only `meta` and partially XDR-decodes each blob to recover the ledger header.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:323-337` — uses that partially decoded header only to build a sequence-presence map.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:344-349` — fully unmarshals the selected blob again before transaction processing.

## Evidence

The helper's partial decode is not needed for response formatting; it exists only because the SQL result omits `sequence`. The sequence is already indexed and available as a normal table column, so paying an XDR parse per selected ledger just to rebuild that information is a byproduct of the current query shape rather than a requirement of the endpoint.

## Anti-Evidence

If the request selects only one or two ledgers, the extra decode is tiny. Full ledger unmarshal, envelope hashing, and transaction serialization still dominate end-to-end cost on dense ledgers, so this is most plausible as a low-severity follow-on optimization or as a multiplier on top of the dense overfetch issue above.
