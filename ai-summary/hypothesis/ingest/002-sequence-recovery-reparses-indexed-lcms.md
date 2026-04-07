# H002: Indexed ledger fetches partially decode every `LedgerCloseMeta` before fully decoding it again

**Date**: 2026-04-07
**Subsystem**: ingest
**Severity**: Low
**Impact**: XDR decode / per-ledger CPU waste
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Because ingest already stores each ledger under `ledger_close_meta.sequence`, `getTransactions` should be able to fetch raw LCM bytes together with their sequence numbers and fully unmarshal each ledger at most once. The hot path should not partially re-parse every BLOB just to rediscover a sequence number SQLite already indexed for it.

## Mechanism

`BatchGetLedgersBySequences()` selects only `meta`, then for every returned row it unmarshals the LCM version, extension, and `LedgerHeaderHistoryEntry` into `chunk.Header`. `getTransactionsByLedgerSequence()` later calls `lcm.UnmarshalBinary(chunk.Lcm)` on the same bytes, so each indexed ledger pays two decode passes; the first one is used only for missing-ledger verification and decode-error context. Returning `(sequence, lcm)` directly from SQLite, or aligning raw `[]byte` results with the already-sorted `ledgerSeqs` input, would remove the first XDR walk entirely.

## Trigger

1. Populate a sparse retained range where many ledgers contain exactly one transaction.
2. Request `getTransactions` with a limit large enough to touch dozens or hundreds of distinct ledgers.
3. Compare the current `BatchGetLedgersBySequences()` path against a version that returns the SQL `sequence` column alongside raw `meta` bytes and skips the partial header decode.

## Target Code

- `cmd/stellar-rpc/internal/db/ledger.go:BatchGetLedgersBySequences:164-203` — decodes version/ext/header from every returned `meta` BLOB.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:339-375` — uses `chunk.Header.Header.LedgerSeq` for verification, then fully unmarshals `chunk.Lcm`.
- `cmd/stellar-rpc/internal/db/sqlmigrations/01_init.sql:14-17` — `ledger_close_meta` already stores `sequence` as a first-class primary key separate from the `meta` blob.

## Evidence

The current reader does real duplicate work: it cannot return a `LedgerMetadataChunk` without first partially parsing each blob, and the handler then fully parses the same bytes before doing any transaction-level work. The SQL table shape already contains the exact sequence number the first parse is recovering, so the extra XDR decode is a reader-API artifact rather than a data-model requirement.

## Anti-Evidence

Header-only decode is cheaper than a full `LedgerCloseMeta` unmarshal, so the absolute savings per ledger are bounded. The optimization matters most when a request spans many distinct ledgers with relatively few returned transactions per ledger; on dense ledgers, later per-transaction encoding costs still dominate.
