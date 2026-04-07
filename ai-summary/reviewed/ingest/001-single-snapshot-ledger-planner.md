# H001: `getTransactions` plans indexed ledgers outside the read transaction it already opened

**Date**: 2026-04-07
**Subsystem**: ingest
**Severity**: Low
**Impact**: DB round-trip / snapshot setup / planner overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once ingest has atomically committed both `transactions` index rows and `ledger_close_meta` rows for a ledger, a single `getTransactions` request should plan candidate ledger sequences and fetch their metadata from the same SQLite read transaction. The hot path should not pay a second DB read path for planner state that belongs to the same committed ingest snapshot.

## Mechanism

`getTransactionsByLedgerSequence()` opens `readTx := h.ledgerReader.NewTx(ctx)`, but it then asks `h.transactionReader.GetLedgerSequencesWithTransactions()` for the planner input through a separate `db.SessionInterface` rather than through `readTx`. That split means each request pays an extra SQLite read/query path and must keep a defensive "index referenced a ledger the read snapshot does not have" verification block. Adding a transaction-index lookup method to `LedgerReaderTx` would let the planner and the LCM fetch share one snapshot and remove that fixed per-request overhead.

## Trigger

1. Run normal tip-following ingest so `transactions` and `ledger_close_meta` are advancing together.
2. Send many small `getTransactions` requests (`limit=1..10`) near the tip while ingest commits are also occurring.
3. Compare the current split planner (`transactionReader` + `readTx`) against a version that performs both the ledger-sequence query and the LCM fetch through the same `readTx`.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:277-385` — opens `readTx`, then calls the planner on `h.transactionReader`, then performs app-side mismatch verification.
- `cmd/stellar-rpc/internal/db/transaction.go:GetLedgerSequencesWithTransactions:170-187` — planner query currently runs through an independent `db.SessionInterface`.
- `cmd/stellar-rpc/internal/db/ledger.go:NewTx:219-233` — the request already pays to open a dedicated read transaction and snapshot cache state.
- `cmd/stellar-rpc/internal/db/db.go:writeTx.Commit:309-368` — ingest commits `transactions` and `ledger_close_meta` atomically, so a single read snapshot can safely observe both.

## Evidence

The handler only needs the transaction index to decide which ledger blobs to fetch, yet it queries that index outside the read transaction it already created for the rest of the request. The extra `len(chunks) != len(ledgerSeqs)` reconciliation block exists because the code knows the planner and fetch phases may observe different committed ingest states, which is both a correctness guard and a visible sign of duplicated DB work.

## Anti-Evidence

This still leaves two SQL statements unless the planner and fetch are fused further, so the gain is a fixed-cost improvement rather than a wholesale redesign. The benefit is most visible for small-limit, high-QPS traffic where request setup costs matter more than per-transaction JSON/XDR formatting.

---

## Review

**Verdict**: VIABLE
**Severity**: Informational
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full `getTransactionsByLedgerSequence` hot path in `get_transactions.go:277-395`. Confirmed that the handler opens an explicit read-only transaction via `h.ledgerReader.NewTx(ctx)` (which clones the DB session, calls `BeginTx` with `ReadOnly: true`, and snapshots the ledger range cache), then queries the planner via `h.transactionReader.GetLedgerSequencesWithTransactions()` which runs through the `transactionHandler.db` field — the bare `daemon.db` `SessionInterface` without an explicit transaction. In SQLite WAL mode, this creates a separate implicit read snapshot. The LCM fetch then runs inside `readTx` via `BatchGetLedgersBySequences`. The two queries can observe different committed states, which is why the reconciliation block at lines 349-362 exists.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:277-395` — confirmed readTx opened at line 280, planner query at line 329 bypasses readTx, reconciliation block at lines 349-362
- `cmd/stellar-rpc/internal/db/transaction.go:GetLedgerSequencesWithTransactions:170-187` — confirmed uses `txn.db.Select()` (bare session, no explicit transaction)
- `cmd/stellar-rpc/internal/db/ledger.go:NewTx:219-233` — confirmed Clone + BeginTx creates an isolated read snapshot with cached ledger bounds
- `cmd/stellar-rpc/internal/db/ledger.go:BatchGetLedgersBySequences:164-204` — confirmed runs via `l.tx.Select()` (within the explicit read transaction)
- `cmd/stellar-rpc/internal/daemon/daemon.go:365-366` — confirmed both `LedgerReader` and `TransactionReader` are constructed from the same `daemon.db`, so they share the underlying connection pool but not transaction scope
- `cmd/stellar-rpc/internal/db/db.go:73-89` — confirmed SQLite WAL mode with auto-checkpoint disabled

### Findings

The inefficiency is real: each `getTransactions` request creates two separate SQLite read snapshots — one explicit (readTx for LCM fetches) and one implicit (transactionHandler for the planner query). The proposed fix of adding a `GetLedgerSequencesWithTransactions` method to `LedgerReaderTx` is correct and would:

1. **Eliminate one implicit SQLite read transaction** per request (~1-5μs savings)
2. **Remove the reconciliation block** (lines 349-362) which allocates a map and iterates when snapshot divergence occurs
3. **Guarantee snapshot consistency** between the planner and fetch phases

However, the severity is **Informational**, not Low, because:

- In SQLite WAL mode, implicit read transactions are extremely cheap (just reading the WAL index header)
- The reconciliation block only allocates when `len(chunks) != len(ledgerSeqs)`, which requires an ingest commit between the two queries — a sub-millisecond race window that almost never triggers
- For a typical `getTransactions` request (500-2000μs total), the 1-5μs savings represents 0.1-1% — below the noise floor of any practical benchmark
- The dominant costs are XDR deserialization and JSON/base64 encoding, not snapshot setup

The fix is correct and improves code hygiene (eliminates a subtle snapshot divergence concern), but would not produce a measurable latency improvement.

### PoC Guidance

- **Target code**: Add `GetLedgerSequencesWithTransactions(ctx, startSeq, endSeq uint32, limit int) ([]uint32, error)` to `LedgerReaderTx` interface in `cmd/stellar-rpc/internal/db/ledger.go` and implement it on `ledgerReaderTx` using `l.tx.Select()`. Then update `get_transactions.go:329-337` to call `readTx.GetLedgerSequencesWithTransactions()` instead of `h.transactionReader.GetLedgerSequencesWithTransactions()`, and remove the reconciliation block at lines 349-362.
- **Change description**: Unify the planner and fetch queries into a single read transaction. The `transactionReader` field can be removed from `transactionsRPCHandler` if `GetTransaction` (used by `getTransaction` singular) is also moved, or kept for that separate path.
- **Correctness check**: Existing tests in `cmd/stellar-rpc/internal/methods/get_transactions_test.go` cover the getTransactions path with various pagination scenarios. The reconciliation block removal is safe because a single snapshot guarantees consistency.
- **Benchmark focus**: Measure p99 latency for `getTransactions` with `limit=1` at high QPS (>5k RPS). Expect <1% improvement — primarily a code-quality win.
