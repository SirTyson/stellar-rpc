# H002: Cursor Pagination Rebuilds the Same Ledger Envelope Map on Every Page

**Date**: 2026-04-07
**Subsystem**: daemon
**Severity**: Medium
**Impact**: repeated hashing / allocation churn
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Sequential `getTransactions` cursor requests that keep walking through the same busy ledger should reuse the already-derived envelope-by-hash mapping for that ledger. Paginating through one ledger in several pages should not re-hash every envelope and rebuild the same map from scratch on each request.

## Mechanism

`transactionsRPCHandler` is effectively stateless, so every request that touches a ledger calls `newLedgerTransactionReader`, which allocates a fresh `map[xdr.Hash]xdr.TransactionEnvelope` and hashes every envelope in `storeTransactions`. The API and integration tests explicitly exercise cursor-based follow-up requests, but there is no hot-ledger cache in the handler or daemon layer. On workloads that page through a single dense ledger with small limits, the daemon pays O(transactions-in-ledger) hashing work on every page instead of once per hot ledger.

## Trigger

Create a ledger with many transactions, then page through it using repeated `getTransactions` calls with `limit=25` or `limit=50`, always feeding the previous response's cursor into the next request. Compare CPU profiles or RPS before and after adding a small LRU keyed by ledger sequence/hash that reuses the parsed envelope map for recently requested ledgers.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:24-31` — handler state contains no cache for parsed hot ledgers.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:289-297` — every request re-enters `processTransactionsInLedger` for each fetched ledger.
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:28-41` — `newLedgerTransactionReader` always allocates a new map and fully initializes reader state.
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:86-103` — `storeTransactions` hashes every envelope in the ledger before any page data is returned.
- `cmd/stellar-rpc/internal/integrationtest/get_transactions_test.go:77-100` — cursor-based follow-up requests are part of the supported access pattern.

## Evidence

The only per-handler memoized field today is the precomputed network ID; there is no structure for retaining parsed ledger readers across requests. `storeTransactions` walks the full envelope list and hashes each entry up front, which means page 2 of the same ledger repeats the exact same work page 1 already did.

## Anti-Evidence

This optimization depends on cross-request locality: if clients mostly ask for one large page or jump across unrelated ledgers, the cache hit rate will be low. It is also a memory-for-CPU tradeoff, so the cache would need a tight size bound to avoid retaining too many large ledgers.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full `getTransactions` cursor pagination path from `getTransactionsByLedgerSequence` → `fetchLedgerMetas` → `processTransactionsInLedger` → `newLedgerTransactionReader` → `storeTransactions`. Confirmed that each request unconditionally builds a new `map[xdr.Hash]xdr.TransactionEnvelope` by iterating all envelopes from `LedgerCloseMeta.TransactionEnvelopes()` (which walks TxSet phases/components) and hashing each via `hashTransactionInEnvelopeWithID` (XDR marshal + SHA-256). The hash map is structurally necessary because TxSet envelope order differs from `TxProcessing` application order — the hash is the only join key. No existing cache or pool mitigates the rebuild.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:275-307` — `getTransactionsByLedgerSequence` iterates `collectedMetas` calling `processTransactionsInLedger` per ledger; no cross-request state
- `cmd/stellar-rpc/internal/methods/get_transactions.go:77-95` — `processTransactionsInLedger` calls `newLedgerTransactionReader` on every invocation
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:28-42` — constructor allocates pre-sized map and calls `storeTransactions`
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:86-104` — `storeTransactions` hashes ALL envelopes unconditionally via `hashTransactionInEnvelopeWithID`
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:111-161` — `hashTransactionInEnvelopeWithID` does XDR marshal to buffer + SHA-256; buffer is reused within one call but not across requests
- `go-stellar-sdk/xdr/ledger_close_meta.go:62-94` — `TransactionEnvelopes()` iterates TxSet phases/components/stages to flatten envelopes; confirms TxSet order ≠ processing order
- `cmd/stellar-rpc/internal/config/options.go:363,369` — production defaults: maxLimit=200, defaultLimit=50
- `cmd/stellar-rpc/internal/methods/get_transactions.go:24-31` — handler struct has no cache fields; only memoized value is `networkID`

### Findings

The inefficiency is **real**: when paginating through a ledger with more transactions than `limit`, each page rebuilds the full envelope hash map. The hash map is necessary for correctness (TxSet order ≠ application order), and no existing optimization mitigates the redundant builds.

**Quantitative estimate**: Hash map construction costs ~2μs per transaction (XDR marshal + SHA-256 with buffer reuse). For a ledger with 100 transactions, rebuild cost is ~200μs. With default limit=50, a 100-tx ledger requires 2 pages, wasting ~200μs on page 2. For a 200-tx ledger with limit=50, 4 pages waste ~400μs × 3 = ~1.2ms total across pages 2-4.

**Why Low, not Medium**: The per-request savings (~200-400μs) represent ~3-8% of total request latency (which is dominated by DB fetches at ~1-5ms and XDR/JSON serialization at ~1-5ms). However, the triggering scenario — paginating through a single dense ledger — requires ledgers with >50 transactions, which is uncommon on mainnet. With the default limit=50, most ledgers are consumed in a single page with no redundancy. The optimization has real but narrow applicability.

**Correctness of proposed fix**: A `sync.Map` or mutex-guarded LRU keyed by ledger sequence is safe because: (1) closed-ledger data is immutable, (2) the map is read-only after construction, (3) `xdr.TransactionEnvelope` values are value types with no aliasing concerns. The cache needs a tight size bound (e.g., 4-8 entries) to avoid retaining large ledger data.

### PoC Guidance

- **Target code**: Add a `readerCache sync.Map` (or small LRU) field to `transactionsRPCHandler` keyed by `uint32` (ledger sequence). In `processTransactionsInLedger`, check cache before calling `newLedgerTransactionReader`; on miss, build and store. Evict entries for ledger sequences outside the current request's range.
- **Change description**: Cache the `map[xdr.Hash]xdr.TransactionEnvelope` across cursor-paginated requests to the same ledger, avoiding redundant `storeTransactions` hashing. The `ledgerTransactionReader` itself is cheap to rebuild (just a struct with readIdx); only the envelope map needs caching.
- **Correctness check**: `cmd/stellar-rpc/internal/methods/get_transactions_test.go` covers cursor pagination (TestGetTransactions_DefaultLimit and cursor-based tests). Integration tests in `cmd/stellar-rpc/internal/integrationtest/get_transactions_test.go` exercise multi-page flows.
- **Benchmark focus**: Create a benchmark that pages through a single ledger with 200 transactions using limit=25 (8 pages). Measure per-page latency and total wall-clock time. Expected improvement: ~400μs per page on pages 2-8, roughly 3-8% per-page latency reduction in XDR format. JSON format pages are heavier so the percentage improvement will be smaller.
