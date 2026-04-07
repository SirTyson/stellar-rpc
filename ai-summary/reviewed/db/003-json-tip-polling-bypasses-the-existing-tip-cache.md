# H003: JSON tip polling still rereads recent ledgers because the tip cache is XDR-only

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Medium
**Impact**: DB read / Rust FFI feed overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once ingestion has the newest ledgers resident in memory, both `xdr` and `json` `getTransactions` requests should avoid re-reading those same ledger blobs from SQLite. The hot tip cache should accelerate repeated JSON polling too, not just the legacy XDR path.

## Mechanism

The code explicitly disables the cache for `format=json`. `lcmCache` stores typed `xdr.LedgerCloseMeta` values, but the JSON fast path needs raw LCM bytes for `lcm_transactions_to_json`, so every JSON request still runs `BatchGetLedgersBySequences()` and partial header decode even when ingestion just appended those ledgers to the in-memory tip cache. A raw-byte companion cache, or a small per-ledger cache of Rust extraction output, would let the JSON fast path skip repeated SQLite blob reads for hot recent ledgers.

## Trigger

Issue repeated `getTransactions(format=json)` requests against the newest few retained ledgers, especially under tip polling. Even though the same ledgers were appended to `lcmCache` at ingestion time, the handler will keep fetching their raw blobs from SQLite and peeling headers before invoking Rust.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:413-466` — cache path is guarded by `request.Format != protocol.FormatJSON`; JSON always fetches raw chunks from DB.
- `cmd/stellar-rpc/internal/ingest/lcm_cache.go:13-19,34-77` — cache stores deserialized `xdr.LedgerCloseMeta`, not raw `[]byte`.
- `cmd/stellar-rpc/internal/ingest/service.go:229-235` — newly ingested ledgers are appended to the tip cache on every commit.
- `cmd/stellar-rpc/internal/db/ledger.go:BatchGetLedgersBySequences:170-211` — JSON fallback re-reads raw blobs and partially decodes headers from SQLite.

## Evidence

The comment in `daemon.MustNew()` says the LCM cache exists so tip-polling `getTransactions` requests can bypass SQLite reads for the newest ledgers, but the live handler only consults it for non-JSON requests (`cmd/stellar-rpc/internal/daemon/daemon.go:193-197`, `cmd/stellar-rpc/internal/methods/get_transactions.go:418-440`). In the JSON branch, the handler always calls `BatchGetLedgersBySequences()`, which pulls the raw blobs from SQLite and peels the header again before Rust reparses the ledger (`cmd/stellar-rpc/internal/db/ledger.go:170-211`, `cmd/stellar-rpc/internal/methods/get_transactions.go:438-469`). The ingestion service is already appending those same ledgers into memory right after commit (`cmd/stellar-rpc/internal/ingest/service.go:229-235`), so the repeated DB read is structural, not accidental.

## Anti-Evidence

The first JSON request for a ledger would still be cold, and any raw-byte or per-ledger JSON cache would increase memory usage for the hottest ledgers. The benefit is concentrated on repeated recent-ledger JSON traffic, not one-off historical scans.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

The LCM cache (`lcm_cache.go`) stores deserialized `xdr.LedgerCloseMeta` values in a `LedgerBucketWindow`. The `getTransactionsByLedgerSequence` handler at line 418 explicitly gates the cache lookup behind `request.Format != protocol.FormatJSON`, so every JSON request falls through to `BatchGetLedgersBySequences` which queries SQLite for raw blobs. The ingestion path at `service.go:232` appends the same ledger to the cache after every commit, confirming the data IS in memory but inaccessible to the JSON path due to the type mismatch (deserialized `xdr.LedgerCloseMeta` vs raw `[]byte` needed by `LCMTransactionsToJSON`).

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:418` — cache guard `request.Format != protocol.FormatJSON` excludes JSON
- `cmd/stellar-rpc/internal/methods/get_transactions.go:438-468` — JSON fallback always calls `readTx.BatchGetLedgersBySequences`
- `cmd/stellar-rpc/internal/ingest/lcm_cache.go:16-18` — cache type is `LedgerBucketWindow[xdr.LedgerCloseMeta]`, not `[]byte`
- `cmd/stellar-rpc/internal/ingest/lcm_cache.go:49-77` — `GetAllLCMs` returns `[]xdr.LedgerCloseMeta`, no raw-byte accessor exists
- `cmd/stellar-rpc/internal/ingest/service.go:229-234` — cache populated after every commit with the deserialized LCM
- `cmd/stellar-rpc/internal/db/ledger.go:170-211` — `BatchGetLedgersBySequences` queries SQLite, copies blob, partial-decodes header
- `cmd/stellar-rpc/internal/methods/get_transactions.go:288` — `xdr2json.LCMTransactionsToJSON(chunk.Lcm)` consumes raw `[]byte`

### Findings

The inefficiency is confirmed: JSON `getTransactions` bypasses the tip cache on every request. The cache stores deserialized Go structs; the JSON path needs raw bytes for Rust FFI. No raw-byte accessor or companion cache exists.

However, the severity is lower than claimed. The SQLite blob read for a hot (recently-written, OS-page-cache-resident) ledger is O(blob_size) but fast — roughly 100-200µs per ledger for a typical LCM. The dominant cost on the JSON path is the Rust FFI call `LCMTransactionsToJSON` which fully parses the LCM and serializes per-transaction JSON, costing 5-20ms per ledger depending on transaction count. The SQLite read savings alone represent ~1-5% of per-ledger JSON processing time.

A raw-byte companion cache would save the SQLite round-trip (query parse, WAL lookup, blob copy) but NOT the Rust FFI. The ingestion path would need one extra `MarshalBinary()` call per ledger to populate the byte cache, since the raw bytes are already gone by the time the cache is populated (the ledger backend returns deserialized `xdr.LedgerCloseMeta`).

A more impactful extension — caching the Rust FFI output (`[]xdr2json.TransactionJSON`) per ledger — would eliminate both the SQLite read and the Rust FFI for repeated requests, yielding much larger savings. This is mentioned in the hypothesis as an alternative.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/ingest/lcm_cache.go` — add a parallel `[]byte` field alongside the `xdr.LedgerCloseMeta` in each bucket, populated by calling `MarshalBinary()` in `Append`. Add a `GetAllRawLCMs(sequences []uint32) ([][]byte, bool)` method. Also modify `cmd/stellar-rpc/internal/methods/get_transactions.go:418-435` to attempt a raw-byte cache lookup for JSON requests before falling through to SQLite.
- **Change description**: Store raw LCM bytes in the tip cache alongside the deserialized form, enabling JSON requests to skip the SQLite read for cached ledgers. The partial header decode can use the same `BatchGetLedgersBySequences` logic inline, or the header fields can be cached alongside the bytes.
- **Correctness check**: Existing `TestGetTransactions` and `TestGetTransactionsNotFound` tests cover the JSON path. The XDR cache path tests also validate cache behavior.
- **Benchmark focus**: p50/p99 latency reduction for `getTransactions(format=json)` on the most recent 1-3 ledgers under repeated polling. Expect ~1-5% improvement from raw-byte caching alone. For a higher-impact PoC, also cache the `LCMTransactionsToJSON` output per ledger.
