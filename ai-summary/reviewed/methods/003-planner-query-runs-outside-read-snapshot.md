# H003: The Indexed Planner Still Pays a Second SQL Round-Trip Outside the Read Snapshot

**Date**: 2026-04-07
**Subsystem**: methods
**Severity**: Medium
**Impact**: latency / DB I/O / wasted retries under ingest
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

After `getTransactions` opens a read transaction, all planning and ledger fetch work should execute against that same snapshot. The handler should not need one query against the global DB handle to find ledger sequences and a second query inside `readTx` to fetch the corresponding metadata.

## Mechanism

`getTransactionsByLedgerSequence()` opens `readTx`, but the transaction-index planner runs through the handler-wide `transactionReader`, which was constructed from the top-level `db.SessionInterface` rather than the read transaction. The request therefore pays two SQL round-trips and materializes an intermediate `[]uint32` ledger list before building a second `IN (...)` query. Under concurrent ingestion, the planner can also observe newer transaction rows than the already-open `readTx` snapshot, which turns the missing-ledger validation into wasted failing work. A tx-scoped planner query or a single joined query inside `LedgerReaderTx` would remove both the extra round-trip and the snapshot mismatch.

## Trigger

1. Run continuous ingestion while issuing `getTransactions` requests against recent ledgers.
2. Observe requests that plan against one snapshot and fetch against an older one, especially around ledger boundaries.
3. Compare latency and failure rate against a version that fetches selected `sequence, meta` rows from the transaction index and `ledger_close_meta` in one read-transaction query.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:254-316` — opens `readTx`, then calls the global `transactionReader` before issuing a second fetch query through `readTx`.
- `cmd/stellar-rpc/internal/db/transaction.go:60-70` — `transactionHandler` stores a standalone `db.SessionInterface`, not a tx-scoped reader.
- `cmd/stellar-rpc/internal/db/transaction.go:GetLedgerSequencesWithTransactions:170-187` — planner query runs on that standalone DB session.
- `cmd/stellar-rpc/internal/db/ledger.go:NewTx:219-233` — the ledger fetch happens inside a separate read-only snapshot.
- `cmd/stellar-rpc/internal/db/ledger.go:BatchGetLedgersBySequences:164-204` — second query reconstructs the selected ledgers via an `IN (...)` list.

## Evidence

The handler deliberately keeps a read transaction open to hold one ledger snapshot, but the planner side-step defeats that design. Because the planner and fetch phases are separated by an intermediate slice of ledger sequences, the code also has to build and validate a second query result set even though SQLite could resolve the same selection directly with a joined, ordered read inside `readTx`.

## Anti-Evidence

On an idle node with no concurrent ingest, the mismatch manifests only as an extra query and some slice/SQL-string construction, so the gain will be smaller. The strongest benefit appears under sustained read/write overlap, where eliminating planner/fetch skew also prevents spurious retry-worthy failures.

---

## Review

**Verdict**: VIABLE
**Severity**: Informational
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full `getTransactionsByLedgerSequence` flow (lines 251–381). The read transaction opens at line 254 via `h.ledgerReader.NewTx(ctx)`, which clones the DB session and calls `BeginTx` with `ReadOnly: true` (ledger.go:219–233). The planner at line 304 calls `h.transactionReader.GetLedgerSequencesWithTransactions`, which runs on the standalone `transactionHandler.db` session (transaction.go:170–187) — a different connection from the read transaction. The fetch at line 315 calls `readTx.BatchGetLedgersBySequences`, running inside the read transaction's snapshot. Both the `InsertLedger` and `InsertTransactions` calls happen in the same write transaction during ingestion (ingest/service.go:297–305), so they're committed atomically. This means the planner can see newly committed ledger sequences that `readTx` doesn't, but can never see transaction rows without the corresponding LCM.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:254` — `h.ledgerReader.NewTx(ctx)` opens a cloned read-only SQL transaction
- `cmd/stellar-rpc/internal/methods/get_transactions.go:304-306` — planner query via `h.transactionReader.GetLedgerSequencesWithTransactions` runs on standalone session
- `cmd/stellar-rpc/internal/methods/get_transactions.go:315` — `readTx.BatchGetLedgersBySequences` runs inside the read transaction snapshot
- `cmd/stellar-rpc/internal/methods/get_transactions.go:324-337` — validation detects mismatch when planner sees ledgers that readTx doesn't
- `cmd/stellar-rpc/internal/db/transaction.go:60-67` — `transactionHandler` holds standalone `db.SessionInterface`
- `cmd/stellar-rpc/internal/db/transaction.go:170-187` — `GetLedgerSequencesWithTransactions` queries via `txn.db.Select`, not within readTx
- `cmd/stellar-rpc/internal/db/ledger.go:219-233` — `NewTx` clones session, begins read-only transaction with `BeginTx`
- `cmd/stellar-rpc/internal/db/ledger.go:164-204` — `BatchGetLedgersBySequences` queries within the transaction's session
- `cmd/stellar-rpc/internal/ingest/service.go:295-310` — ingestion atomically commits both LCM and transaction rows in one write tx
- `cmd/stellar-rpc/internal/daemon/daemon.go:365-366` — both `LedgerReader` and `TransactionReader` are constructed from the same `daemon.db` but use independent sessions

### Findings

1. **The inefficiency exists.** The planner query (`SELECT DISTINCT ledger_sequence FROM transactions WHERE ...`) runs on a standalone DB session outside the read transaction, adding one extra SQL round-trip per `getTransactions` request. This is confirmed by tracing `transactionHandler.db` (transaction.go:63) which stores the bare `db.SessionInterface` passed to `NewTransactionReader`, while `readTx` uses a cloned session with `BeginTx` (ledger.go:222–223).

2. **It is in a hot path.** Every `getTransactions` call executes this planner query. At 200 RPS (the sustainable load from the batch optimization benchmark), this is 200 extra SQL round-trips per second.

3. **The snapshot mismatch is real but extremely rare.** If ingestion commits between the `NewTx()` call (line 254) and the planner query (line 304) — a window of microseconds to low milliseconds — the planner can see ledger sequences that `readTx` doesn't have. This triggers the validation at lines 324–337, returning an error. However, the ingestion commit interval is ~5 seconds, so the probability of hitting this window is vanishingly small.

4. **The per-invocation cost is minimal.** The planner query is `SELECT DISTINCT ledger_sequence FROM transactions WHERE ledger_sequence >= ? AND ledger_sequence <= ? ORDER BY ledger_sequence ASC LIMIT ?` — an indexed scan returning at most ~201 uint32 values. On SQLite in WAL mode (in-process), this completes in sub-millisecond time. Against the optimized handler baseline of ~7ms for sparse scans (per success/methods/001 benchmarks), this represents roughly 1–4% of handler time.

5. **The proposed fix is architecturally correct but requires non-trivial refactoring.** Moving the planner query inside `readTx` would require either: (a) adding a `GetLedgerSequencesWithTransactions` method to `LedgerReaderTx`, which currently only queries the `ledger_close_meta` table, breaking its single-table abstraction; or (b) using a JOIN query (`ledger_close_meta JOIN transactions`) inside `LedgerReaderTx`, coupling the two tables; or (c) having `transactionHandler` accept a session parameter per call. All approaches are correct but change the current interface boundaries.

6. **Severity downgrade.** The hypothesis claims Medium (5–20%), but the actual per-request savings (sub-millisecond from eliminating one indexed query) translate to <5% of handler time even in the best case. The snapshot mismatch causes wasted work only under extreme timing coincidences. The improvement would be in the noise for production workloads with network latency and serialization overhead.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence` (lines 301–312) and `cmd/stellar-rpc/internal/db/ledger.go:LedgerReaderTx` interface (lines 35–45)
- **Change description**: Add a `GetLedgerSequencesWithTransactions(ctx, startSeq, endSeq, limit) ([]uint32, error)` method to `LedgerReaderTx` that queries the `transactions` table using the transaction's session (`l.tx`). Then replace `h.transactionReader.GetLedgerSequencesWithTransactions(...)` at line 304 with `readTx.GetLedgerSequencesWithTransactions(...)`. Alternatively, implement a single JOIN query `SELECT lcm.meta FROM ledger_close_meta lcm JOIN (SELECT DISTINCT ledger_sequence FROM transactions WHERE ...) t ON lcm.sequence = t.ledger_sequence ORDER BY lcm.sequence ASC` inside a new `LedgerReaderTx` method, eliminating both the extra round-trip and the intermediate `[]uint32` materialization.
- **Correctness check**: Existing tests in `cmd/stellar-rpc/internal/methods/get_transactions_test.go` cover pagination, cursor handling, dense/sparse scans, and format output. The mock for `LedgerReaderTx` in `mocks.go` will need the new method added.
- **Benchmark focus**: Micro-benchmark `getTransactionsByLedgerSequence` with the sparse-scan benchmark from success/methods/001. Expected improvement: <5% latency reduction (sub-millisecond savings on ~7ms baseline). The snapshot mismatch elimination is a correctness refinement, not a performance metric.
