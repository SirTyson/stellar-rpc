# H003: Ingest-managed recent-ledger lifecycle is unused, so repeated pollers re-encode identical `getTransactions` results after every close

**Date**: 2026-04-07
**Subsystem**: ingest
**Severity**: Medium
**Impact**: CPU / xdr2json / base64 / allocation overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

After a new ledger is ingested, repeated `getTransactions` calls for that same hot recent ledger should be able to reuse already formatted transaction results until the ledger ages out of the recent window. Pollers hitting the newest ledger should not force the server to rebuild identical `TransactionInfo` payloads for every request.

## Mechanism

The ingest subsystem already defines the exact lifecycle of "hot recent ledgers" but currently retains only `latestIngestedSeq`; the request path always starts from raw `LedgerCloseMeta` and reruns `ParseTransaction()`, `transactionToJSON()`, `jsonifySlice()`, event encoding, and base64 conversion for every hit. A recent-ledger cache owned by the ingest window could lazily memoize per-ledger `[]protocol.TransactionInfo` (or per-format variants) on the first request, then reuse those slices for subsequent pollers until ingest appends new ledgers and evicts the old ones. That avoids the flaw in prior "precompute on ingest" ideas because the expensive formatting work is still paid only when a ledger is actually queried.

## Trigger

1. Run normal tip-following ingest.
2. After each ledger close, issue many `getTransactions` requests against the newest ledger or a very small recent range, especially with `format=json`.
3. Compare the current path against a version that lazily memoizes per-ledger responses in an ingest-evicted recent window.

## Target Code

- `cmd/stellar-rpc/internal/ingest/service.go:ingest:190-242` — ingest establishes the hot-ledger frontier but retains only the latest sequence.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:139-227` — every request rebuilds `TransactionInfo` and re-encodes results/events.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:304-368` — repeated pollers always walk ledgers again; there is no recent-ledger memoization layer.
- `cmd/stellar-rpc/internal/db/transaction.go:ParseTransaction:238-321` — request path repeatedly marshals transaction result/meta/envelope/events to bytes.
- `cmd/stellar-rpc/internal/methods/json.go:transactionToJSON/jsonifySlice/jsonifySliceOfSlices:12-90` — JSON requests repeatedly cross the FFI boundary and rebuild identical JSON payloads.
- `cmd/stellar-rpc/internal/methods/get_transaction.go:BuildEventsXDRFromTransaction/BuildEventsJSONFromTransaction:133-155` — event formatting is also repeated per request.
- `cmd/stellar-rpc/internal/ledgerbucketwindow/ledgerbucketwindow.go:25-120` — existing bounded recent-ledger container is suitable for cache ownership and eviction.
- `cmd/stellar-rpc/internal/daemon/daemon.go:220-225` — backfill already resets caches, giving a clean invalidation boundary for a hot-ledger response cache.

## Evidence

There is no memoization layer anywhere in the `getTransactions` path: each request reconstructs the same response objects from scratch even when many clients are polling the same latest ledger immediately after close. The codebase already has the ingredients to make such caching safe — contiguous recent-ledger windows, explicit backfill reset points, and a clear ingest-driven eviction boundary — but it currently uses those only for scalar metadata and fee stats.

## Anti-Evidence

The first request for a ledger sees no benefit, and memory usage could become significant for dense ledgers or if both JSON and XDR variants are cached. The cache design must preserve cursor semantics and avoid copying very large response slices unnecessarily.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated. Distinct from failed ingest/H003 (which incorrectly claimed ingest already does ParseTransaction work and proposed precomputation during ingest) because this hypothesis uses lazy memoization on first request. Distinct from reviewed ingest/H001 (LCM cache) because this caches formatted TransactionInfo output, not raw LedgerCloseMeta blobs — these are additive optimizations at different layers.

### Trace Summary

Traced the full `getTransactions` request path from `getTransactionsByLedgerSequence` (get_transactions.go:269-378) through `processTransactionsInLedger` (get_transactions.go:76-234). Confirmed that every request constructs a new `ledgerTransactionReader` (which hashes all envelopes via SHA-256), iterates all transactions, and for JSON format calls `ParseTransaction` (3× `MarshalBinary` + event extraction), then `transactionToJSON` (3× CGo/FFI calls to Rust xdr2json), `jsonifySlice` (1× batch CGo call for diagnostic events), and `BuildEventsJSONFromTransaction` (2× CGo calls). For XDR format, `MarshalBase64` via `xdr.EncodingBuffer` is used per field. None of this work is cached or shared across concurrent requests for the same ledger. The inefficiency is real and the lazy caching mechanism is sound.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:269-378` — `getTransactionsByLedgerSequence` opens a read transaction, batch-fetches LCMs via `BatchGetLedgerMetas`, and loops over ledgers calling `processTransactionsInLedger`. No cache check exists anywhere in this path.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:76-234` — `processTransactionsInLedger` constructs `ledgerTransactionReader` (which hashes all envelopes at line 88), iterates transactions, and for each transaction rebuilds `TransactionInfo` from scratch with format-specific encoding (JSON at lines 150-186, XDR at lines 188-219).
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:28-42` — `newLedgerTransactionReader` builds an `envelopesByHash` map by hashing every transaction envelope via SHA-256 (line 38, `storeTransactions`). This per-ledger hashing cost is incurred on every request even when the same ledger was just processed by another concurrent request.
- `cmd/stellar-rpc/internal/db/transaction.go:238-278` — `ParseTransaction` performs 3× `MarshalBinary` (Result, Meta, Envelope) plus event extraction via `GetTransactionEvents` and `parseEvents`. Each `MarshalBinary` allocates a new `[]byte`.
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:36-43,64-126` — `ConvertBytes` and `ConvertBytesSlice` cross the CGo boundary to invoke Rust xdr2json. Each call involves C memory allocation (`C.CBytes`), Rust XDR parsing, JSON serialization, and Go string copy (`C.GoString`). These FFI crossings are the most expensive per-transaction operations on the JSON path.
- `cmd/stellar-rpc/internal/methods/json.go:12-37,56-58` — `transactionToJSON` calls `ConvertBytes` 3× (Result, Envelope, Meta). `jsonifySlice` delegates to `ConvertBytesSlice` for batch conversion.
- `cmd/stellar-rpc/internal/ledgerbucketwindow/ledgerbucketwindow.go:10-120` — Generic `LedgerBucketWindow[T]` with `Append`, `Get`, contiguity enforcement, and eviction. Could host a `LedgerBucketWindow[cachedLedgerTransactions]` keyed by format.

### Findings

1. **The inefficiency is confirmed and the mechanism is correct.** Every `getTransactions` request for a recent ledger pays the full processing cost: envelope hashing (SHA-256 per envelope), `ParseTransaction` (3× MarshalBinary + event extraction), format-specific encoding (CGo/FFI for JSON, MarshalBase64 for XDR). None of this is shared across concurrent requests for the same ledger. A lazy cache keyed by (ledger sequence, format) that stores the complete `[]protocol.TransactionInfo` for a ledger would eliminate all of this for cache hits.

2. **JSON format benefits most.** The JSON path involves 3+ CGo boundary crossings per transaction (via `transactionToJSON` and `ConvertBytesSlice`), each requiring C memory allocation, Rust XDR parsing, JSON serialization, and Go string copying. The XDR path uses `xdr.EncodingBuffer.MarshalBase64` which reuses an internal buffer — still allocates output strings but avoids FFI overhead.

3. **Cursor handling is feasible but requires full-ledger processing.** The cache must store the complete per-ledger transaction list (all transactions, not just those matching a specific cursor position). The first request that triggers cache population must process ALL transactions in the ledger, even if its own cursor+limit only requires a subset. Subsequent requests slice into the cached list based on `start.TransactionOrder`. This means the first request may do slightly more work than today (processing past its limit), but all subsequent requests become O(1) lookups.

4. **Severity downgrade to Low.** The optimization only helps when multiple requests hit the same ledger before it is evicted from the cache window. For single-poller or historical-scan workloads, there is zero benefit. For high-concurrency tip-polling with JSON format, the improvement could be 5-15% of aggregate throughput. But this is scenario-dependent and incremental on top of the already-reviewed H001 (LCM cache), which addresses the DB fetch + XDR deserialization cost at a lower layer. The combination of H001 + this cache would cover the full request path, but the marginal gain of this second layer is smaller than H001's standalone benefit.

5. **Architectural note.** Despite the hypothesis framing this as "ingest-owned," the cache properly belongs in the methods/handler layer since `protocol.TransactionInfo` is a methods-layer type. The ingest subsystem provides the eviction signal (new ledger sequence), but the cache itself serves the request path. A `sync.RWMutex`-guarded `LedgerBucketWindow[[]protocol.TransactionInfo]` per format (or a combined struct) attached to `transactionsRPCHandler` would be the natural home.

### PoC Guidance

- **Target code**: Add a response cache to `transactionsRPCHandler` in `cmd/stellar-rpc/internal/methods/get_transactions.go`. Introduce a cache struct (e.g., `type ledgerTxCache struct { mu sync.RWMutex; xdrCache, jsonCache *LedgerBucketWindow[[]protocol.TransactionInfo] }`) with a small window size (e.g., 10 ledgers per format). In `processTransactionsInLedger`, check the cache before processing; on miss, process the full ledger and store the result. On hit, slice the cached `[]TransactionInfo` based on `start.TransactionOrder` and `limit`.
- **Change description**: (1) Define a cache keyed by (ledger sequence, format) holding `[]protocol.TransactionInfo`. (2) In `getTransactionsByLedgerSequence`, before calling `processTransactionsInLedger`, check if the ledger is cached for the requested format. (3) On cache miss, process the full ledger (ignoring limit for cache population), store the result, then slice for the caller's cursor+limit. (4) On cache hit, slice the cached result. (5) Wire cache eviction to ingest's latest sequence (e.g., via a callback or periodic check against `latestLedgerSeq`).
- **Correctness check**: Existing tests in `cmd/stellar-rpc/internal/methods/get_transactions_test.go` cover pagination, cursor semantics, and format variants. The cache must produce identical results to the uncached path for all cursor positions and limits. Backfill reset (daemon.go) must clear the cache.
- **Benchmark focus**: Measure `getTransactions` p50/p99 latency and aggregate RPS under 10-50 concurrent pollers all requesting `startLedger=latest, limit=200, format=json`. Cache hit rate and per-request CPU time should be the primary metrics. Expect <5% end-to-end improvement for mixed workloads, potentially higher for pure tip-polling with JSON format.
