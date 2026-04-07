# H001: Tip-Polling `getTransactions` Pages Re-Serialize Immutable Ledgers on Every Request

**Date**: 2026-04-07
**Subsystem**: methods
**Severity**: Medium
**Impact**: latency / CPU / allocations
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When many clients repeatedly poll or paginate through the same recent ledgers, `getTransactions` should be able to reuse already-serialized per-ledger transaction payloads instead of rebuilding identical `TransactionInfo` values on every request. Once a retained ledger has been committed, its transaction set and both format-specific encodings are immutable, so repeated low-limit reads of that ledger should mostly be slicing cached results rather than re-running transaction parsing and serialization.

## Mechanism

The current cache only memoizes the envelope-by-hash map needed to reconcile tx-set order with processing order. On a cache hit, `processTransactionsInLedger()` still walks the same ledger transactions, rebuilds `TransactionInfo`, base64-encodes result/meta/envelope/events for XDR responses, or runs `db.ParseTransaction()` plus `batchConvertTransactionsToJSON()` for JSON responses. Because the code already documents repeated same-ledger polling as a common workload, a bounded recent-ledger cache keyed by `(ledger sequence, format)` should eliminate a much larger repeated CPU/allocation surface than the current envelope-only cache.

## Trigger

1. Run continuous tip-following ingestion so the latest few ledgers are queried repeatedly.
2. Send many `getTransactions` requests with `startLedger` at or near the latest retained ledger and small limits (for example 1-10), or paginate the same dense ledger across multiple requests.
3. Compare current latency and allocation profiles against a version that caches fully built `[]protocol.TransactionInfo` slices (or equivalent immutable serialized rows) per recent ledger and slices them by cursor/limit on cache hits.

## Target Code

- `cmd/stellar-rpc/internal/methods/envelope_cache.go:9-18` — the existing cache explicitly targets repeated scans of the same tip ledgers, but only memoizes envelope maps.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:78-209` — every request still rebuilds `TransactionInfo`, event payloads, and response fields for each touched transaction.
- `cmd/stellar-rpc/internal/methods/json.go:101-210` — JSON pages still batch-convert the same immutable XDR payloads on every repeated ledger read.

## Evidence

The codebase already acknowledges the repeated-ledger polling pattern in `envelope_cache.go`, and the current implementation stops caching at the narrowest sub-step: envelope hashing. Everything after `reader.Read()` in `processTransactionsInLedger()` is deterministic for a committed ledger and repeated verbatim on every request, including XDR encoding (`MarshalBase64`) and JSON conversion batching. That makes the remaining repeated work unusually cache-friendly compared with the already-optimized envelope-map step.

## Anti-Evidence

This only helps workloads with substantial overlap across requests; wide historical scans and one-off sparse walks still need the uncached path. A viable implementation also needs a bounded window and explicit eviction on retention movement so cached transaction payloads never outlive their underlying ledgers.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full `getTransactions` hot path from `getTransactionsByLedgerSequence` through `BatchGetLedgersBySequences` (SQL fetch + partial XDR unmarshal), full `lcm.UnmarshalBinary`, `newLedgerTransactionReader` (envelope cache check), and `processTransactionsInLedger` (per-tx field extraction + format-specific serialization). Confirmed that on every request for the same ledger, even with an envelope cache hit, the code re-fetches the LCM blob from SQLite, re-unmarshals the entire LCM, re-iterates all transactions, and re-serializes every field (XDR: 4-5 `MarshalBase64` calls per tx; JSON: `MarshalBinary` × 3 + event extraction per tx, then page-level CGo batch conversion). The envelope cache only eliminates the ~2-7µs/envelope hashing step, leaving the dominant cost (DB I/O + unmarshal + serialization) fully repeated.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:304-306` — `GetLedgerSequencesWithTransactions` index query identifies relevant ledgers (cheap, needed regardless)
- `cmd/stellar-rpc/internal/methods/get_transactions.go:315` — `BatchGetLedgersBySequences` fetches LCM blobs from SQLite; each blob is ~10-500KB depending on transaction density
- `cmd/stellar-rpc/internal/db/ledger.go:164-204` — SQL `SELECT meta FROM ledger_close_meta WHERE sequence IN (...)` + partial XDR unmarshal for header extraction; repeated on every request
- `cmd/stellar-rpc/internal/methods/get_transactions.go:344-346` — full `lcm.UnmarshalBinary(chunk.Lcm)` deserializes entire LCM including all transaction envelopes, results, metas; repeated on every request
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:30-62` — `newLedgerTransactionReader` checks envelope cache; on hit, skips hashing but returns a reader that still wraps the freshly-unmarshaled LCM
- `cmd/stellar-rpc/internal/methods/get_transactions.go:152-196` — format switch: XDR path calls `enc.MarshalBase64` 4-5 times per tx + diagnostic/contract event encoding; JSON path calls `db.ParseTransaction` (3× `MarshalBinary` + event extraction per tx)
- `cmd/stellar-rpc/internal/db/transaction.go:276-316` — `ParseTransaction` marshals Result, Meta, Envelope to binary + extracts all events; this is the dominant per-tx cost on the JSON path
- `cmd/stellar-rpc/internal/methods/json.go:105-211` — `batchConvertTransactionsToJSON` makes 6 CGo calls for the page (already batched, but still repeated for the same ledger data)

### Findings

The inefficiency is real and the proposed fix is architecturally sound. The per-request repeated work for a cached ledger includes:

**Per-ledger fixed costs (repeated on every request):**
1. SQLite fetch of LCM blob: ~100-500µs (depends on blob size and OS page cache state)
2. Full XDR unmarshal of LCM: ~50-500µs (depends on transaction density)
3. Envelope cache check: ~1µs on hit (negligible)

**Per-transaction costs (repeated for every tx in every request):**
- XDR path: `MarshalBase64` × 4 fields (~5-20µs/tx) + diagnostic/contract event encoding (~2-10µs/tx per event)
- JSON path: `MarshalBinary` × 3 fields (~3-15µs/tx) + event extraction (~2-10µs/tx) + CGo batch conversion (amortized ~5-20µs/tx)

**Total per-ledger per-request cost (50-tx ledger):**
- XDR: ~500-1500µs (DB + unmarshal) + ~350-1500µs (serialization) ≈ **850-3000µs**
- JSON: ~500-1500µs (DB + unmarshal) + ~250-1250µs (ParseTransaction) + CGo batch ≈ **1000-4000µs**

A response-level cache keyed by `(ledger_sequence, format)` would reduce these to a single map lookup + array slice operation (~1µs), saving essentially the entire cost for cached ledgers.

**Severity downgrade rationale (Medium → Low):**

1. **Workload dependency**: The benefit only materializes for repeated-access patterns (tip-polling). The blaster benchmark uses diverse requests across the retention window; a tip-polling-specific benchmark is needed.

2. **Previous benchmark evidence**: H011 (envelope hashing optimization, structurally similar but narrower) and H012 (redundant event extraction) both showed no measurable end-to-end improvement in blaster benchmarks despite addressing real inefficiencies in the same code path. This suggests the getTransactions processing cost may not be the dominant latency contributor under realistic mixed workloads.

3. **Memory overhead**: Caching full `TransactionInfo` slices adds significant memory pressure. Each ledger's cache entry holds all serialized strings/bytes for all transactions. At 50 tx × ~10-20KB/tx = ~0.5-1MB per ledger, a 64-ledger cache = ~32-64MB. This is manageable but not free, and the capacity might need to be smaller than the existing envelope cache.

4. **Implementation complexity**: The cache check must happen before `BatchGetLedgersBySequences` to avoid the DB fetch. This requires restructuring `getTransactionsByLedgerSequence` to split the ledger list into cache hits (skip DB) and misses (fetch from DB), process misses, populate cache, then combine results. The JSON path loses cross-ledger batching on cache misses (per-ledger conversion instead), though cache hits skip conversion entirely.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/methods/get_transactions.go` — the `getTransactionsByLedgerSequence` method (lines 251-381) and `processTransactionsInLedger` (lines 78-209); also a new cache type analogous to `envelope_cache.go`
- **Change description**: 
  1. Create a `transactionInfoCache` (similar design to `envelopeCache` — bounded FIFO ring) keyed by `(uint32 ledgerSeq, string format)` storing `[]protocol.TransactionInfo` per ledger.
  2. In `getTransactionsByLedgerSequence`, after `GetLedgerSequencesWithTransactions` returns the ledger list, partition it into cache hits and cache misses by checking the new cache.
  3. For cache hits: retrieve the pre-built `[]TransactionInfo` and slice by cursor position (transaction order maps directly to array index since `TransactionInfo.ApplicationOrder` is 1-indexed).
  4. For cache misses: fetch LCMs via `BatchGetLedgersBySequences`, process via `processTransactionsInLedger` as today, then populate the cache with the full ledger's `[]TransactionInfo` before applying cursor/limit slicing.
  5. For JSON format: on cache miss, convert per-ledger (not page-level batching) and cache the post-conversion `TransactionInfo`. On cache hit, results already have JSON fields populated.
  6. Use a smaller default capacity than the envelope cache (e.g., 32 ledgers × 2 formats = 64 entries) to bound memory.
- **Correctness check**: Existing `getTransactions` tests in `get_transactions_test.go` (pagination, cursor resume, JSON format, multi-ledger) must pass unchanged. The cached `TransactionInfo` must be identical to freshly-computed ones — verify by comparing a few ledgers both ways. Ensure cache eviction when the retention window moves past cached ledgers (the FIFO ring naturally handles this if capacity ≤ retention window size).
- **Benchmark focus**: The standard blaster benchmark may not show improvement due to diverse request patterns. A targeted micro-benchmark or modified blaster config focusing on repeated tip-ledger polling (many requests hitting the same 2-3 latest ledgers) should show the clearest signal. Measure per-request latency reduction for repeated identical `getTransactions` calls to the same ledger. Expect ~50-80% latency reduction for cache hits on individual requests, translating to a smaller aggregate improvement depending on cache hit rate.
