# H001: Hot recent ledgers still rebuild envelope order even though ingest already walked every transaction

**Date**: 2026-04-07
**Subsystem**: ingest
**Severity**: Medium
**Impact**: CPU / envelope hashing / allocation overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When `getTransactions` serves a recently ingested ledger, especially with a small `limit` or a cursor deep into the ledger, it should not need to hash every envelope in that ledger before it can return the first requested transaction. The hot path should be able to reuse the application-order envelope mapping that ingest already discovered while indexing that same ledger.

## Mechanism

`newLedgerTransactionReader()` eagerly calls `storeTransactions()`, which hashes every envelope and builds `envelopesByHash` for the entire ledger before `Seek()` or `Read()` can return anything. `getTransactions` pays that O(txCount) setup cost on every request touching the ledger, even when the request only needs one or two transactions. During ingest, `InsertTransactions()` already walks the ledger sequentially and computes the same transaction hashes and application-order information to populate the lookup table, so an ingest-populated recent ordered-envelope cache (for example `[]xdr.TransactionEnvelope` keyed by ledger sequence) could let hot requests skip the full rehash/map-build phase.

## Trigger

1. Ingest a run of dense recent ledgers.
2. Call `getTransactions` against the newest ledger with `limit=1..5`, or with a cursor late in the ledger.
3. Compare the current path against a version that reuses an ingest-populated ordered-envelope slice for recent ledgers before falling back to `newLedgerTransactionReader()`.

## Target Code

- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:newLedgerTransactionReader:28-41` — request path allocates a fresh reader and eagerly builds per-ledger envelope state.
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:storeTransactions:86-104` — hashes every envelope up front to populate `envelopesByHash`.
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:hashTransactionInEnvelopeWithID:111-160` — per-envelope hashing work repeated for every request.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:88-129` — request path constructs the reader before it knows how many transactions it will actually return.
- `cmd/stellar-rpc/internal/db/transaction.go:InsertTransactions:68-143` — ingest already performs the sequential transaction walk and computes outer/inner hashes plus application order.
- `cmd/stellar-rpc/internal/ledgerbucketwindow/ledgerbucketwindow.go:NewLedgerBucketWindow/Get/GetLedgerRange:25-120` — existing bounded contiguous-ledger window suitable for a recent ordered-envelope cache.

## Evidence

The hot request path always rebuilds `envelopesByHash` from scratch, and it does so before `Seek()` can skip ahead to the requested cursor. In parallel, ingest already pays for a full ordered transaction walk in `InsertTransactions()` to compute the transaction-index data that powers `getTransaction` lookup, so the system has a natural place to retain a lightweight recent-ledger ordering artifact without adding a new full-ledger scan.

## Anti-Evidence

This only helps recently ingested ledgers and mostly when ledgers are dense enough that envelope hashing is a noticeable share of request time. It does not remove the later `ParseTransaction()` / event / encoding costs, so the end-to-end win is bounded unless it is paired with other hot-ledger caching.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated (distinct from reviewed ingest/H001 LCM cache which skips SQLite+unmarshal but still requires envelope hashing; distinct from failed ingest/H003 which proposed caching full ParseTransaction output)

### Trace Summary

Traced the full envelope hashing path from `processTransactionsInLedger` (get_transactions.go:88) → `newLedgerTransactionReader` (ledger_transaction_reader.go:28-41) → `storeTransactions` (ledger_transaction_reader.go:86-104) → `hashTransactionInEnvelopeWithID` (ledger_transaction_reader.go:111-160). Confirmed that the hashing is eager and unconditional: every request touching a ledger hashes ALL envelopes in that ledger before `Seek()` or `Read()` can proceed. The hash-based lookup is structurally necessary because `TransactionEnvelopes()` returns envelopes in transaction-set order (from `V1TxSet.Phases` structure), while `TransactionHash(i)` returns hashes in application/processing order (from `TxProcessing[i].Result`). These orders can differ in generalized transaction sets (protocol v20+), making the hash map the only correct way to match envelopes to their application-order indices.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:28-41` — `newLedgerTransactionReader` pre-allocates the map at capacity `CountTransactions()` and immediately calls `storeTransactions(networkID)`. No lazy-loading path exists.
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:86-104` — `storeTransactions` iterates `TransactionEnvelopes()` (set order), hashes each via `hashTransactionInEnvelopeWithID`, and inserts into `envelopesByHash`. Uses a reusable `bytes.Buffer` and pre-computed `networkID` (optimizations already applied).
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:111-160` — Per-envelope work: constructs `TransactionSignaturePayload` (networkID + tagged transaction body), XDR-marshals to buffer, SHA-256 hashes the result. For V0 envelopes, also constructs a V1 wrapper. Each hash costs ~5-20μs (marshal + SHA-256 of ~0.5-5KB payload).
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:44-74` — `Read()` uses `lcm.TransactionHash(i)` to get the application-order hash, then looks up `envelopesByHash[txHash]` to get the corresponding envelope. This two-step lookup is why the hash map exists.
- `go-stellar-sdk@v0.4.0/xdr/ledger_close_meta.go:62-94` — `TransactionEnvelopes()` flattens envelopes from the generalized transaction set structure (phases → components/stages → clusters). Order follows the TxSet structure, NOT application order.
- `go-stellar-sdk@v0.4.0/xdr/ledger_close_meta.go:97-108` — `TransactionHash(i)` returns `TxProcessing[i].Result.TransactionHash` — the hash at application-order index `i`. Different ordering than `TransactionEnvelopes()`.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:88-94` — `processTransactionsInLedger` constructs a new reader per ledger. For a batch of 50 ledgers, this means 50 full envelope-hashing passes.
- `cmd/stellar-rpc/internal/db/transaction.go:88-110` — `InsertTransactions` uses the SDK's `ingest.NewLedgerTransactionReaderFromLedgerCloseMeta` which performs the same envelope hashing internally (less optimized — recomputes networkID from passphrase per envelope). Both ingest and request paths hash all envelopes; neither retains the mapping.

### Findings

1. **The inefficiency is real and structurally necessary.** The `envelopesByHash` map cannot be eliminated because `TransactionEnvelopes()` and `TransactionHash(i)` use different orderings. Every request that touches a ledger must build this map to correctly pair envelopes with their application-order results.

2. **The cost is proportionally significant for small-limit tip-polling.** For a dense mainnet ledger with 100-200 transactions:
   - Envelope hashing: ~100-200 × 10μs = 1-2ms per ledger
   - Per-transaction processing (limit=1-5): 0.5-5ms for XDR format, 1-10ms for JSON
   - The hashing setup is 10-67% of per-ledger work for small limits on dense ledgers
   - For large limits (100+), the per-transaction processing dominates and hashing is <5%

3. **The custom reader already has optimizations.** Pre-computed `networkID` (handler field, get_transactions.go:29), reused `bytes.Buffer` (ledger_transaction_reader.go:87), and pre-allocated map (ledger_transaction_reader.go:34) are already applied. Further optimization of the hashing itself has diminishing returns — the win comes from avoiding it entirely via caching.

4. **Complementary to reviewed ingest/H001 (LCM cache).** The LCM cache (VIABLE/Low) eliminates SQLite read + XDR deserialization but still requires `newLedgerTransactionReader` → `storeTransactions` → full envelope hashing. This hypothesis targets the next layer of redundant work. If both are implemented, the combined benefit for near-tip small-limit polling would be meaningful.

5. **Severity downgrade to Low.** The hypothesis claims Medium (5-20% latency reduction), but for the average `getTransactions` call the improvement is <5%. The 10-67% per-ledger savings only materializes for limit=1-5 on dense ledgers, and the absolute savings (1-2ms) are small. For typical multi-ledger or transaction-heavy requests, the per-transaction processing (base64/JSON encoding, ParseTransaction marshaling) dominates.

6. **Simpler alternatives to ingest coupling exist.** The hypothesis proposes an ingest-populated cache, but the ingest path uses the SDK reader (different implementation than the custom reader). Coupling ingest to request handling is architecturally complex. Simpler approaches:
   - **Handler-level LRU cache**: Cache `map[uint32]map[xdr.Hash]xdr.TransactionEnvelope` (keyed by ledger sequence) with a small capacity (10-50 entries). Multiple concurrent requests for the same ledger share one hash computation.
   - **Extend the LCM cache**: If reviewed/H001's LCM cache is implemented, augment it to store `(LedgerCloseMeta, map[xdr.Hash]TransactionEnvelope)` pairs, so the envelope map is built once during ingest and reused.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/methods/get_transactions.go` (handler struct and `processTransactionsInLedger`) and `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go` (`newLedgerTransactionReader`).
- **Change description**: Add a `sync.RWMutex`-guarded LRU cache of `map[xdr.Hash]xdr.TransactionEnvelope` keyed by ledger sequence to `transactionsRPCHandler`. In `processTransactionsInLedger`, before calling `newLedgerTransactionReader`, check the cache. On hit, construct the reader with the pre-built map (add an alternative constructor). On miss, build the map normally and insert into the cache. Alternatively, if reviewed/H001's LCM cache is implemented first, extend that cache to store the envelope map alongside the LCM.
- **Correctness check**: Existing tests in `cmd/stellar-rpc/internal/methods/get_transactions_test.go` cover the request path. The cache is transparent — same `LedgerTransaction` output either way. Envelope ordering correctness depends on hash matching, which is deterministic. Cache entries should be evicted when the retention window advances (same lifecycle as the LCM cache).
- **Benchmark focus**: Measure `getTransactions` p50/p99 latency for `startLedger = latest, limit = 1-5` with dense ledgers (100+ txns) under concurrent polling. The envelope hashing portion (measurable via `storeTransactions` timing) should drop to near-zero on cache hits. Expect <5% end-to-end improvement for average workloads, potentially 10-30% for single-ledger small-limit polling on dense ledgers.
