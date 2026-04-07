# H001: Index Planner Overfetches Dense `getTransactions` Pages

**Date**: 2026-04-07
**Subsystem**: methods
**Severity**: High
**Impact**: latency / DB I/O / allocations
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once `getTransactions` knows the request limit and starting cursor, it should fetch only the non-empty ledgers needed to fill the page. A dense page such as `limit=1`, `limit=10`, or even `limit=200` should not prefetch `limit+1` distinct ledgers when the first one or two ledgers already contain enough transactions.

## Mechanism

The current index-driven planner bounds `GetLedgerSequencesWithTransactions()` by `int(limit)+1`, which is a transaction-count heuristic applied to **ledger** selection. On dense traffic, one selected ledger can contribute dozens or hundreds of transactions, yet `BatchGetLedgersBySequences()` still copies and partially decodes every selected ledger before `processTransactionsInLedger()` gets a chance to stop on the first full page. That reintroduces dense-page overfetch under the new planner shape and can easily dominate low-limit requests near the tip.

## Trigger

1. Populate recent ledgers densely enough that one or two ledgers satisfy a page (for example, 50-100 transactions per ledger).
2. Call `getTransactions` with `startLedger` near the latest ledger and `limit=1`, `limit=10`, or `limit=200`.
3. Compare latency and bytes allocated against a version that probes a small number of non-empty ledgers first and only expands if the page is still short.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:301-359` — queries up to `limit+1` distinct ledgers, fetches all of them, and only then starts per-ledger processing.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:203-205` — the page can become complete inside the first processed ledger, after the prefetch has already happened.
- `cmd/stellar-rpc/internal/db/transaction.go:GetLedgerSequencesWithTransactions:170-187` — limits the planner by count of ledger sequences rather than by actual remaining transactions.
- `cmd/stellar-rpc/internal/db/ledger.go:BatchGetLedgersBySequences:162-204` — materializes every selected ledger blob and partially decodes its header before the caller can stop early.

## Evidence

The old dense-page overfetch issue was tied to a fixed contiguous batch; the current code has the same shape with a different planner. The request asks for transactions, but the planner selects up to `limit+1` **ledgers** without consulting cursor position inside the first ledger or observed transaction density, so a `limit=200` page can still fetch 201 non-empty ledger blobs even when only 2 ledgers are actually needed.

## Anti-Evidence

Sparse one-transaction-per-ledger workloads genuinely need close to `limit` distinct ledgers, so the current planner is well matched there. The gain is concentrated on dense ledgers and small/medium limits, not on the sparse historical scan workload that motivated the index-driven redesign.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the complete index-driven `getTransactions` path in the current codebase. The handler calls `GetLedgerSequencesWithTransactions(ctx, startSeq, lastSeq, int(limit)+1)` at line 304, which issues `SELECT DISTINCT ledger_sequence ... LIMIT ?` with the transaction-count limit applied as a ledger-count cap. All returned sequences are passed to `BatchGetLedgersBySequences` (line 315), which reads every raw blob from SQLite and partially decodes headers. The processing loop (lines 339-358) then lazily fully-decodes each LCM and breaks on page completion. For dense ledgers (50-100 txns each) with `limit=200` (production max), up to 201 non-empty ledger blobs are read from SQLite when only 2-3 are needed. This is a distinct inefficiency from the previously rejected lazy-decode optimization (reviewed/methods/001-fixed-batch-overfetches-dense-pages, which changed decode strategy for the SAME number of blobs); this hypothesis targets the NUMBER of blobs fetched.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:304-306` — `GetLedgerSequencesWithTransactions(ctx, uint32(start.LedgerSequence), lastLedgerSeq, int(limit)+1)` passes `limit+1` as the SQL LIMIT on distinct ledger sequences, treating it as a ledger count when it's actually a transaction count
- `cmd/stellar-rpc/internal/methods/get_transactions.go:315` — `readTx.BatchGetLedgersBySequences(ctx, ledgerSeqs)` fetches ALL returned sequences in a single SQL query with a large IN clause
- `cmd/stellar-rpc/internal/db/ledger.go:164-204` — `BatchGetLedgersBySequences` reads raw blobs for all sequences, allocates `[][]byte` on the Go heap, and partially decodes XDR headers for every blob
- `cmd/stellar-rpc/internal/methods/get_transactions.go:339-358` — inner loop lazily fully-decodes each chunk via `lcm.UnmarshalBinary(chunk.Lcm)` and breaks when page limit is met (`done` at line 356)
- `cmd/stellar-rpc/internal/methods/get_transactions.go:203-205` — page completion check inside `processTransactionsInLedger`: `len(*txns) >= limitInt` returns `done=true`
- `cmd/stellar-rpc/internal/db/transaction.go:170-187` — `GetLedgerSequencesWithTransactions` issues `SELECT DISTINCT ledger_sequence ... LIMIT ?` with the passed limit
- `cmd/stellar-rpc/internal/config/options.go:360-364` — production `max-transactions-limit` defaults to 200; `default-transactions-limit` defaults to 50

### Findings

**The inefficiency is real and structurally confirmed.** The code passes `int(limit)+1` — a transaction-count heuristic — as the SQL LIMIT on distinct ledger sequences. For dense workloads (Stellar mainnet can have 50-200 txns/ledger during bursts), this means:

- `limit=200` (max): 201 non-empty ledger sequences queried, 201 blobs fetched, ~198 wasted when 2-3 ledgers suffice
- `limit=50` (default): 51 sequences, ~49 wasted
- `limit=1`: 2 sequences, 1 wasted (minor)

**Cost breakdown for the wasted blobs (limit=200, 100 txns/ledger, ~100-300KB per blob):**
1. SQLite IN-clause query reads ~198 unnecessary blobs from page cache → 20-60 MB of memory copies to Go heap
2. Partial XDR header decode of ~198 blobs (cheap but non-zero)
3. Go heap allocation pressure and GC load from the oversized `[][]byte` and `[]LedgerMetadataChunk` slices

**Severity downgrade from High to Low:**
The hypothesis claims High severity (>20% improvement). However, several factors limit the practical impact:

1. **Prior related rejection:** `reviewed/methods/001-fixed-batch-overfetches-dense-pages` (which became `fail/db/011-fixed-batch-eager-decode`) proposed a lazy-decode optimization that avoided full XDR deserialization of excess ledgers in the OLD code path. Independent benchmarks showed it was 5-10% SLOWER, not faster. While that was a decode-strategy change (not a fetch-count change), it demonstrates that reducing work on excess ledgers doesn't necessarily translate to end-to-end improvement.

2. **Adaptive fetching adds round-trip overhead:** An adaptive approach (fetch small initial set, expand if needed) requires 2-4 SQL query pairs instead of 2 for the sparse case. Each extra query adds fixed overhead (~9μs per query from benchmark data). For sparse workloads that need most of the `limit` ledgers, the extra round-trips could offset the dense-case savings.

3. **SQLite page cache mitigates I/O cost:** For recent ledgers near the tip (the primary dense-ledger use case), blobs are likely in SQLite's page cache. Reading cached pages is fast — the dominant cost is the memory copy to Go heap and GC, not disk I/O.

4. **Mixed-workload dilution:** The blaster benchmark tests a mix of dense and sparse queries. Dense-case improvements are diluted by no-change or slight regression on sparse queries.

**Net assessment:** The overfetch is real and the waste is measurable in isolation (blob reads, allocations), but end-to-end latency impact in mixed workloads is likely <5%. The fix is correct in principle but needs careful implementation and benchmarking to avoid the same fate as the prior rejected optimization.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence` (lines 301-359)
- **Change description**: Replace the single-shot `GetLedgerSequencesWithTransactions(..., int(limit)+1)` + `BatchGetLedgersBySequences(all)` with an adaptive two-phase approach:
  1. First probe: query a small initial set of ledger sequences (e.g., `min(int(limit)+1, 10)`)
  2. Fetch and process those ledgers
  3. If page not full, query remaining sequences starting from where the first batch left off and repeat
  
  A simpler alternative: cap the initial ledger fetch at a small constant (e.g., 20) and only expand if the page is not filled. This minimizes changes and avoids complex density estimation.

  ```go
  const initialLedgerBatch = 20
  ledgerLimit := min(int(limit)+1, initialLedgerBatch)
  ledgerSeqs, err := h.transactionReader.GetLedgerSequencesWithTransactions(
      ctx, uint32(start.LedgerSequence), lastLedgerSeq, ledgerLimit,
  )
  // ... process ...
  // If !done and more sequences may exist:
  if !done && len(ledgerSeqs) == ledgerLimit {
      lastSeq := ledgerSeqs[len(ledgerSeqs)-1]
      moreSeqs, err := h.transactionReader.GetLedgerSequencesWithTransactions(
          ctx, lastSeq+1, lastLedgerSeq, int(limit)+1-ledgerLimit,
      )
      // ... fetch and process moreSeqs ...
  }
  ```

- **Correctness check**: All existing tests in `cmd/stellar-rpc/internal/methods/get_transactions_test.go` must pass. Cursor pagination, limit enforcement, and gap validation logic are all independent of the number of ledgers fetched per round. The key invariant to preserve: the response cursor must correctly reflect the position of the last returned transaction.
- **Benchmark focus**: Run `stellar-rpc-blaster` at matched load (75/100 RPS) comparing baseline vs adaptive on the same machine. Key metric: p50/p95 latency for `getTransactions`. Also run a targeted micro-benchmark with dense ledgers (50-100 txns/ledger) and `limit=1,10,50,200` to isolate the dense-case improvement. Target: measurable but modest improvement (<5% end-to-end); any regression at any load level means the optimization is not viable for production.
