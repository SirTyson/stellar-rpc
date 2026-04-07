# H017: Combining the planner query and ledger fetch into one SQL statement is too small to matter

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Low
**Impact**: DB round-trip overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

`getTransactions` should avoid extra SQL statements only when the statement overhead is a meaningful share of request time. If the planner/fetch split is expensive, the handler should combine those steps so one query identifies the target ledgers and returns the corresponding blobs.

## Mechanism

I suspected the new index-driven path might still leave fixed DB overhead because it first runs `GetLedgerSequencesWithTransactions()` and then issues a second query to fetch or stream the matching ledgers. If that extra statement and intermediate `[]uint32` slice were significant, replacing the split flow with one CTE/join query could trim request latency.

## Trigger

Issue many small `getTransactions` requests and focus on the DB phase, comparing time in the planner query versus the subsequent `BatchGetLedgersBySequences()` / `StreamLedgersBySequences()` query.

## Target Code

- `cmd/stellar-rpc/internal/db/transaction.go:GetLedgerSequencesWithTransactions:171-200` — planner query returns distinct ledger sequences for the page.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:403-476` — handler materializes `ledgerSeqs` and then runs a second query to fetch the actual ledger blobs.
- `cmd/stellar-rpc/internal/db/ledger.go:BatchGetLedgersBySequences:170-211` — JSON path fetches raw blobs in a second statement.
- `cmd/stellar-rpc/internal/db/ledger.go:StreamLedgersBySequences:253-284` — XDR path streams deserialized ledgers in a second statement.

## Evidence

The split flow is real in current code: the handler first obtains `ledgerSeqs` from the transactions index, then separately fetches the ledgers identified by those sequences (`cmd/stellar-rpc/internal/methods/get_transactions.go:403-476`). That means one extra SQL statement and one intermediate `[]uint32` allocation per request.

## Anti-Evidence

The second query still has to read the large `ledger_close_meta.meta` blobs and, on the XDR path, fully deserialize them. Consolidating the statements would remove only a small planner round-trip and a tiny integer slice, while leaving the dominant blob I/O, XDR decode, Rust conversion, and per-transaction response work untouched.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The split-query shape is structurally redundant, but the removable work is just one extra SQL statement and a small `[]uint32` result. The expensive part of the request is still moving and processing the ledger blobs themselves, so merging the statements would not produce a meaningful endpoint win without a much larger change.

### Lesson Learned

For the post-planner `getTransactions` path, DB optimizations must remove blob reads, XDR decode, or repeated per-ledger CPU. Pure statement-count cleanup is too small once the index has already reduced the ledger set.
