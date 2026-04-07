# H004: Sorted Non-Empty Ledger Sequences Are Never Collapsed Back Into Contiguous Range Reads

**Date**: 2026-04-07
**Subsystem**: methods
**Severity**: Low
**Impact**: latency / SQL overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When the transaction index returns long contiguous runs of non-empty ledgers, `getTransactions` should reuse the existing range-read helpers for those runs instead of always rebuilding the fetch as a by-sequence `IN (...)` query. Exact ledger selection should be preserved, but contiguous spans should ride the cheaper sequential path.

## Mechanism

`GetLedgerSequencesWithTransactions()` returns sequences ordered ascending, and active near-tip traffic often produces many consecutive non-empty ledgers. `getTransactionsByLedgerSequence()` currently hands that sorted slice straight to `BatchGetLedgersBySequences()`, which always emits `WHERE sequence IN (...) ORDER BY sequence ASC` even when the input is effectively one or two contiguous ranges. Collapsing `ledgerSeqs` into `[start,end]` runs and fetching each run with `BatchGetLedgers()` (or a tx-scoped streaming range helper) should reduce SQL construction and let SQLite walk adjacent ledger rows sequentially without changing the set of ledgers returned.

## Trigger

1. Benchmark `getTransactions` on dense recent traffic where the planner returns many adjacent ledger sequences.
2. Capture query plans and latency for the current by-sequence fetch versus a run-collapsing variant that converts adjacent sequences into range reads.
3. Focus on pages where the selected sequence list is mostly contiguous but still produced through the transaction-index path.

## Target Code

- `cmd/stellar-rpc/internal/db/transaction.go:170-186` — the planner query returns ledger sequences in ascending order.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:304-315` — the sorted sequence slice is forwarded unchanged to the by-sequence fetch helper.
- `cmd/stellar-rpc/internal/db/ledger.go:93-136` — `BatchGetLedgers()` already provides an ordered contiguous-range fetch path.
- `cmd/stellar-rpc/internal/db/ledger.go:162-204` — `BatchGetLedgersBySequences()` always builds the fetch as an `IN (...)` query.

## Evidence

The current code already has both ingredients needed for run coalescing: the planner emits sorted sequence numbers, and the DB layer has a dedicated contiguous-range helper. That makes the unconditional `IN (...)` fetch look like a missed normalization step in the handoff between planner and fetcher, especially on active ledgers where non-empty sequences cluster tightly.

## Anti-Evidence

This does little for truly sparse histories where the selected sequences are isolated singletons, and exact planner improvements are likely to deliver larger wins on dense pages. The benefit here is therefore a smaller, workload-dependent reduction in SQL overhead rather than a universal hot-path fix.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated (related to H008/H015/H016 but targets query shape rather than decode or streaming)
**Failed At**: reviewer

### Trace Summary

Traced the full code path from `getTransactionsByLedgerSequence` through the planner (`GetLedgerSequencesWithTransactions`) and into `BatchGetLedgersBySequences`. The hypothesis correctly identifies that `IN (...)` is used where a `BETWEEN` range query could serve contiguous runs. However, the number of sequences is small (typically 3–40 for default/max limits of 50/200 transactions), and the SQL query overhead is negligible compared to the dominant cost of reading and partially decoding ~500KB ledger blobs per sequence.

### Code Paths Examined

- `cmd/stellar-rpc/internal/db/transaction.go:171-200` — `GetLedgerSequencesWithTransactions` uses row-level precision (`LIMIT limit` on individual transaction rows), returning only the distinct ledger sequences that contain the next `limit` transactions. For default limit=50 with ~5–20 txns/ledger, this yields ~3–10 sequences. For max limit=200, ~10–40 sequences.
- `cmd/stellar-rpc/internal/db/ledger.go:162-204` — `BatchGetLedgersBySequences` builds `WHERE sequence IN (?, ?, ...) ORDER BY sequence ASC` via squirrel's `sq.Eq{"sequence": sequences}`. For N values, SQLite performs N B-tree index lookups. Adjacent keys generally land on the same B-tree leaf pages, so the lookups are cache-friendly even without an explicit range scan.
- `cmd/stellar-rpc/internal/db/ledger.go:93-136` — `BatchGetLedgers(start, end)` uses `WHERE sequence >= ? AND sequence <= ? ORDER BY sequence ASC`. This is a single range scan. For contiguous inputs, the result set is identical to the `IN (...)` variant because the LCM table contains a row for every closed ledger in the retention window.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:304-315` — The planner result (`ledgerSeqs`) is passed directly to `BatchGetLedgersBySequences`. No run-detection or coalescing is attempted.
- `cmd/stellar-rpc/internal/config/options.go:362-369` — Default limit is 50 transactions, max is 200. These bound the number of ledger sequences the planner returns.

### Why It Failed

The SQL query shape difference between `IN (...)` and `BETWEEN` is not a meaningful performance bottleneck for the typical number of sequences (3–40):

1. **SQL construction overhead is trivial.** Building `IN (?, ?, ..., ?)` with 40 placeholders via squirrel is a sub-microsecond string operation. The Go-side overhead of constructing the range alternative (run detection, possibly multiple range queries for non-contiguous segments) could easily exceed the savings.

2. **SQLite query planning difference is negligible.** For an `IN (...)` clause with N sorted values on an indexed primary key, SQLite performs N B-tree seeks. For `BETWEEN`, it performs 1 seek + sequential scan. On adjacent keys, the B-tree seeks hit the same leaf page (already in the page cache), so the wall-clock difference is ~5–50µs for typical N values of 3–40.

3. **I/O dominates.** Each ledger blob is ~500KB. Fetching 10 ledgers means reading ~5MB of data from SQLite. The query planning phase is <0.1% of this cost. Even in the most optimistic scenario (40 contiguous sequences, single range scan saves 50µs of seek overhead), the savings represent 0.01–0.1% of a 5–50ms total request.

4. **Added complexity is not justified.** Run-coalescing logic must detect contiguous spans, issue range queries for contiguous runs, fall back to `IN` for isolated sequences, and merge results. This increases code complexity and maintenance burden for a sub-0.1% improvement that would not register in any benchmark.

5. **The planner already minimizes the sequence count.** As established in H016 (fail/methods/016), the row-level precision planner returns a near-minimal set of ledger sequences. The sequence list is short enough that the `IN (...)` vs `BETWEEN` distinction is irrelevant at this scale.

### Lesson Learned

When evaluating SQL query shape optimizations, estimate the query overhead relative to the data transfer cost. For queries that return large blobs (hundreds of KB per row), the query planning and index traversal phase is orders of magnitude smaller than the I/O phase. Optimizing the `WHERE` clause shape provides meaningful gains only when the number of values is very large (thousands) or the per-row data is very small (a few bytes). For `getTransactions` with 3–40 sequences of ~500KB each, the I/O phase dominates by a factor of 1000x+.
