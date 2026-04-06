# H003: Empty Ledgers Still Build an Ingest Reader Before the Handler Learns There Is Nothing to Read

**Date**: 2026-04-06
**Subsystem**: methods
**Severity**: Medium
**Impact**: CPU / allocations
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When a scanned ledger contains zero transactions, `getTransactions` should skip directly to the next ledger without constructing an `ingest.LedgerTransactionReader`. Empty-ledger scans should be almost free once the metadata is loaded.

## Mechanism

`processTransactionsInLedger` calls `ingest.NewLedgerTransactionReaderFromLedgerCloseMeta` before it checks `ledger.CountTransactions()`. For empty ledgers, the function then sets `txCount := ledger.CountTransactions()` and immediately skips the loop, meaning reader construction and any internal setup/allocation work were pure overhead. The pattern is avoidable: `db/event.go` already guards on `txCount == 0` before opening a transaction reader for event ingestion.

## Trigger

1. Load a retention window with a large run of empty ledgers.
2. Call `getTransactions` with `startLedger` before that run and a small limit so the handler must traverse it.
3. Compare CPU samples or allocation counts before and after moving the `CountTransactions()` fast path ahead of reader construction.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:99-130` — reader construction happens before the zero-transaction check.
- `cmd/stellar-rpc/internal/db/event.go:76-85` — event ingestion shows an existing `txCount == 0` guard before opening the reader.

## Evidence

The current ordering is visible directly in the hot path: reader creation at line 104, `txCount := ledger.CountTransactions()` at line 128, and the read loop only starts at line 130. A sibling subsystem already treats zero-transaction ledgers as a cheap early-return case, which suggests the same fast path was simply missed here.

## Anti-Evidence

This helps only when the scanned range contains many empty ledgers; dense ledgers still need the reader. It also does not address the per-ledger SQL lookup cost, so its benefit compounds best when paired with a better ledger-selection strategy.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-06
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated
**Failed At**: reviewer

### Trace Summary

Traced the full reader construction path from `processTransactionsInLedger` (get_transactions.go:104) through `NewLedgerTransactionReaderFromLedgerCloseMeta` (go-stellar-sdk/ingest/ledger_transaction_reader.go:48-62) and into `storeTransactions` (lines 125-150). For empty ledgers, the reader construction allocates one struct and one empty map (`make(map[xdr.Hash]xdr.TransactionEnvelope, 0)` — Go does not allocate buckets for zero-capacity maps), then `storeTransactions` calls `TransactionEnvelopes()` which returns an empty slice, so the hashing loop executes zero iterations. The total cost per empty ledger is ~50-100 nanoseconds.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:104` — calls `NewLedgerTransactionReaderFromLedgerCloseMeta`
- `go-stellar-sdk@v0.4.0/ingest/ledger_transaction_reader.go:48-62` — allocates struct + empty map, calls `storeTransactions`
- `go-stellar-sdk@v0.4.0/ingest/ledger_transaction_reader.go:125-150` — `storeTransactions` iterates `TransactionEnvelopes()`, which returns empty slice for empty ledgers — zero iterations
- `go-stellar-sdk@v0.4.0/xdr/ledger_close_meta.go:49-60` — `CountTransactions()` is a simple `len()` on a slice field — trivially cheap
- `cmd/stellar-rpc/internal/methods/get_transactions.go:265` — `fetchLedgerData` performs a SQL query per ledger (~100-1000µs), dominating per-ledger cost by 1000x+

### Why It Failed

The inefficiency exists but is negligibly small relative to the dominating cost in the same loop. For empty ledgers, reader construction costs ~50-100 nanoseconds (one struct allocation + one empty map header + one empty-slice iteration). Meanwhile, `fetchLedgerData` at line 265 performs a SQL query per ledger costing ~100-1000 microseconds — 1000-10000x more expensive. Even scanning 1000 consecutive empty ledgers, the reader construction overhead totals ~100µs against 100-1000ms of SQL fetches, representing 0.01-0.1% of total execution time. This is far below any measurable threshold and well below the Informational cutoff.

### Lesson Learned

When evaluating allocation-avoidance optimizations, always trace through to the actual allocation cost (not just the number of calls). Go's `make(map, 0)` is essentially free, and an empty-range iteration has no loop body to execute. The real bottleneck for empty-ledger scanning in getTransactions is the per-ledger SQL fetch, not the reader construction.
