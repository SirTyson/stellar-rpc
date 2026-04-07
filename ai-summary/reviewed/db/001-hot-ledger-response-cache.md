# H001: Repeated tip polling rebuilds the same immutable ledger responses from scratch

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: High
**Impact**: CPU / allocation / DB read overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once a retained ledger has been ingested, repeated `getTransactions` requests over that same ledger window should reuse previously prepared transaction response data. A hot tip-polling workload should not re-read the same SQLite blob, rebuild the ledger transaction reader, and re-encode the same transactions on every request.

## Mechanism

The current read path has no cache below `latestLedgerSeq` / `latestLedgerCloseTime`, so every request reconstructs transaction response state from immutable ledger data even when clients repeatedly page over the same recent ledgers. Because the retention window is bounded and `writeTx.Commit()` already knows exactly when ledgers fall out of scope, a bounded per-ledger cache keyed by `(ledger sequence, format)` or by raw parsed transaction slices could turn repeated tip polling into mostly slice/cursor work instead of repeated DB/XDR/JSON work.

## Trigger

Issue repeated `getTransactions` requests with small limits against the most recent few ledgers, or page through the same dense retained ledgers from multiple clients. Profiles should show the same ledgers repeatedly paying `BatchGetLedgerMetas()`, `newLedgerTransactionReader()`, `db.ParseTransaction()`, and JSON/XDR encoding costs on every request.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:267-377` — every request reopens a read tx, re-fetches ledgers, and rebuilds response items
- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:76-233` — per-ledger transaction reader setup plus per-transaction encoding happen on every call
- `cmd/stellar-rpc/internal/db/db.go:dbCache:49-54` — the only in-memory cache today tracks latest-ledger metadata, not reusable transaction results
- `cmd/stellar-rpc/internal/db/db.go:writeTx.Commit:301-352` — commit/trim path already defines a bounded retention window that can invalidate cached ledgers safely

## Evidence

`getTransactionsByLedgerSequence()` always fetches ledger blobs through a new `LedgerReaderTx` and then calls `processTransactionsInLedger()` for each request (`cmd/stellar-rpc/internal/methods/get_transactions.go:272-377`). Inside that helper, the code always rebuilds the per-ledger transaction reader and re-encodes each returned transaction (`cmd/stellar-rpc/internal/methods/get_transactions.go:88-233`). The DB cache only stores the latest ledger sequence and close time (`cmd/stellar-rpc/internal/db/db.go:49-54`), while `writeTx.Commit()` already has the exact trim boundary needed to evict stale cache entries deterministically (`cmd/stellar-rpc/internal/db/db.go:301-352`).

## Anti-Evidence

The first request touching a ledger would still pay the current full cost, so this only helps workloads with repeated access to the same retained ledgers. Any cache has to preserve the current cursor semantics and avoid duplicating too much memory for both JSON and XDR formats.

---

## Review

**Verdict**: VIABLE
**Severity**: Medium
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full `getTransactions` read path from `getTransactionsByLedgerSequence` through `BatchGetLedgerMetas` (SQLite SELECT + full XDR unmarshal), `newLedgerTransactionReader` (SHA-256 hash of every envelope to build lookup map), and `processTransactionsInLedger` (per-transaction field extraction + base64/JSON encoding). Confirmed that `dbCache` only stores `latestLedgerSeq` and `latestLedgerCloseTime` — no response-level caching exists anywhere. Verified that `writeTx.Commit` trims ledgers outside the retention window, providing a clean eviction boundary. All output is deterministic for a given `(ledger sequence, format)` pair, making the data safely cacheable.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:267-377` — opens read tx, fetches batches of 50 ledgers via `BatchGetLedgerMetas`, iterates with `processTransactionsInLedger`. No caching of any intermediate or final results.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:76-233` — creates `ledgerTransactionReader` (hashes all envelopes), then for each transaction: extracts fields, marshals to base64 (XDR) or runs `db.ParseTransaction` + FFI `xdr2json` (JSON). All work repeated identically on every request for the same ledger.
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:newLedgerTransactionReader:28-42` — allocates `envelopesByHash` map, calls `storeTransactions` which iterates all envelopes and SHA-256 hashes each. This is O(N) hash operations per ledger per request.
- `cmd/stellar-rpc/internal/db/ledger.go:BatchGetLedgerMetas:120-140` — SQL SELECT + full `xdr.LedgerCloseMeta` unmarshal. SQLite page cache helps with I/O but not with XDR deserialization.
- `cmd/stellar-rpc/internal/db/db.go:dbCache:49-54` — only `latestLedgerSeq` and `latestLedgerCloseTime`. No transaction/response data cached.
- `cmd/stellar-rpc/internal/db/db.go:writeTx.Commit:301-353` — trims ledgers/transactions/events by retention window, then atomically commits + updates cache. Provides deterministic eviction boundary usable for cache invalidation.

### Findings

The inefficiency is **confirmed and real**. Every `getTransactions` request reconstructs the full response from raw SQLite blobs, even when the same ledgers were processed moments ago by a previous request. The per-ledger cost includes:

1. **SQLite read + XDR unmarshal** of the full `LedgerCloseMeta` blob (typically 10s–100s of KB per ledger)
2. **Envelope hashing** — SHA-256 of every transaction envelope to build the `envelopesByHash` map (O(N) hashes per ledger)
3. **Per-transaction encoding** — base64 marshaling (XDR format) or `ParseTransaction` + FFI to Rust `xdr2json` (JSON format)

All of this output is deterministic for a given `(ledger sequence, format)` pair, and ledger data is immutable once ingested. The retention window provides a natural, well-defined eviction boundary.

**Severity downgrade from High to Medium**: The improvement is workload-dependent. For a pure tip-polling workload with high cache hit rates, savings could exceed 20%. However, for mixed workloads (varied ledger ranges, first-time access), the benefit is smaller. Additionally, several other reviewed optimizations (H001 fixed-batch-eager-decode, H005 raw-lcm-json-extraction) would independently reduce per-request cost, lowering the marginal value of caching. Memory overhead is also a concern — each cached ledger's response data could be 100KB–2MB depending on transaction density, requiring careful LRU sizing.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/methods/get_transactions.go` — add an LRU cache to `transactionsRPCHandler` keyed by `(ledger_sequence, format_string)`, storing `[]protocol.TransactionInfo` per ledger.
- **Change description**: Before calling `processTransactionsInLedger`, check the cache for each ledger sequence + format. On cache hit, slice directly into the cached `[]TransactionInfo` using the cursor position. On miss, process normally and populate the cache. Use `sync.RWMutex` for thread safety. Bound the cache to a configurable number of ledgers (e.g., 200) to limit memory. Evict entries when `writeTx.Commit` trims ledgers (or simply rely on LRU eviction). A simpler alternative: cache only the deserialized `[]xdr.LedgerCloseMeta` to avoid repeated SQLite reads and XDR unmarshal, deferring per-transaction encoding to each request.
- **Correctness check**: Existing tests in `cmd/stellar-rpc/internal/methods/get_transactions_test.go` cover pagination, cursor semantics, and format handling. Verify these still pass with caching enabled.
- **Benchmark focus**: Measure latency and allocations for repeated `getTransactions` calls against the same recent ledger range. Expect 30-60% latency reduction on cache-hot workloads (XDR format) and 15-40% on JSON format (where FFI cost dominates). Use `go test -bench` with a warm cache scenario vs cold.
