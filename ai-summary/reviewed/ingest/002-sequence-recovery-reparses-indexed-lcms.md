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

---

## Review

**Verdict**: VIABLE
**Severity**: Informational
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

`BatchGetLedgersBySequences` (ledger.go:164-203) selects only the `meta` column, then for each row performs a sequential XDR unmarshal of the version discriminant, optionally the `LedgerCloseMetaExt` union, and the full `LedgerHeaderHistoryEntry` — all to populate `chunk.Header`. The sole consumer in `getTransactions` (get_transactions.go:349-374) uses only `chunk.Header.Header.LedgerSeq` for a missing-ledger count check and for error-message context before performing the full `lcm.UnmarshalBinary(chunk.Lcm)`. Since the `ledger_close_meta` table has `sequence` as its primary key, the partial decode is genuinely redundant — the sequence number could come directly from SQL.

### Code Paths Examined

- `cmd/stellar-rpc/internal/db/ledger.go:164-203` — `BatchGetLedgersBySequences` selects only `meta`, builds a `bytes.Reader`, unmarshals version (Int32), optionally `LedgerCloseMetaExt` (union discriminant + void for v0), and `LedgerHeaderHistoryEntry` (~300 bytes of XDR including two 32-byte hashes, timestamps, fee info, etc.)
- `cmd/stellar-rpc/internal/methods/get_transactions.go:349-362` — missing-ledger verification uses `c.Header.Header.LedgerSeq` to build a map; could use a SQL-returned sequence instead
- `cmd/stellar-rpc/internal/methods/get_transactions.go:369-374` — full `UnmarshalBinary` of the same bytes; the partial decode result is not passed through
- `cmd/stellar-rpc/internal/methods/get_ledgers.go:196` and `json.go:48` — `BatchGetLedgers` (the range variant) DOES use `chunk.Header` for actual response data and JSON conversion, so only `BatchGetLedgersBySequences` can be simplified
- `cmd/stellar-rpc/internal/db/sqlmigrations/01_init.sql:14-15` — `sequence INTEGER NOT NULL PRIMARY KEY` confirms the column is available

### Findings

The inefficiency is confirmed: `BatchGetLedgersBySequences` performs a partial XDR decode (version + extension + LedgerHeaderHistoryEntry) on every returned blob, but the `getTransactions` caller only uses the decoded `LedgerSeq` field — a value already stored as the SQL primary key. The fix (selecting `sequence` alongside `meta` and returning it as a struct field) is correct and would not break any callers.

However, the **absolute cost is negligible relative to total request time**:
- The partial decode parses ~340 bytes per ledger (4-byte version + 4-byte ext discriminant + ~330-byte header entry)
- The full `UnmarshalBinary` that follows parses the entire LCM blob, which for a ledger with even modest transaction volume is 10KB–1MB+
- The partial decode is therefore <1% of the per-ledger XDR decode cost
- Downstream costs (processTransactionsInLedger, newLedgerTransactionReader with envelope hashing, ParseTransaction, JSON encoding via xdr2json FFI) are orders of magnitude larger
- For a 50-ledger request, the total savings would be on the order of microseconds against milliseconds of total processing

The `LedgerCloseMetaExt` in practice is usually v0 (void), so its decode is just reading a 4-byte discriminant. Even when v1, it doesn't change the magnitude assessment.

**Severity downgraded from Low to Informational**: while the redundancy is real and the fix is trivially correct, the savings are too small to produce a measurable latency change on `getTransactions`. The optimization is a code-cleanliness improvement (removing an unnecessary dependency on partial XDR parsing) rather than a performance win.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/db/ledger.go:BatchGetLedgersBySequences` — change SQL to `SELECT sequence, meta`, return a new struct (or add a `Sequence uint32` field to `LedgerMetadataChunk`) that carries the SQL sequence, and skip the partial XDR decode loop. Update `cmd/stellar-rpc/internal/methods/get_transactions.go:349-374` to use `chunk.Sequence` instead of `chunk.Header.Header.LedgerSeq`.
- **Change description**: Select `sequence` column alongside `meta` in `BatchGetLedgersBySequences`, populate it from SQL, remove the partial header decode. Update the two `getTransactions` call sites that reference `chunk.Header.Header.LedgerSeq`.
- **Correctness check**: `cmd/stellar-rpc/internal/db/ledger_test.go` (BenchmarkBatchGetLedgers), `cmd/stellar-rpc/internal/methods/get_transactions_test.go`, and mock expectations in `mocks.go:96` for `BatchGetLedgersBySequences`. Note that `BatchGetLedgers` (range variant) must keep its partial decode since `get_ledgers.go` and `json.go` use `chunk.Header` for response data.
- **Benchmark focus**: `BenchmarkBatchGetLedgers` (or a new `BenchmarkBatchGetLedgersBySequences`) — expect per-ledger improvement in the low-microsecond range, likely <1% of total getTransactions latency. The benchmark would need to isolate just the batch-get function to see the difference.
