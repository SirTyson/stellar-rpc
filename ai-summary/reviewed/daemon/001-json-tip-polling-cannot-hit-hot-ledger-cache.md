# H001: JSON Tip Polling Cannot Hit the Hot-Ledger Cache

**Date**: 2026-04-07
**Subsystem**: daemon
**Severity**: Medium
**Impact**: redundant SQLite reads / JSON-mode tip latency
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When `getTransactions` repeatedly polls the newest ledgers in `format=json`, the daemon should be able to serve those ledgers from the same in-memory hot-ledger cache that already exists for the non-JSON path. Recent ledgers that ingest just fetched should not be reread from SQLite on every JSON request while they are still resident in memory.

## Mechanism

The daemon wires `getTransactions` to an `LCMCache`, but that cache stores only typed `xdr.LedgerCloseMeta` values. The JSON path is explicitly excluded from the cache fast path because `processChunksJSON` needs raw `LedgerCloseMeta` bytes for `xdr2json.LCMTransactionsToJSON`, so every JSON tip request still opens a DB read transaction and rereads the same `meta` BLOBs from SQLite. Caching raw LCM bytes alongside the typed value (or caching a dual representation) would let JSON-mode tip polling reuse the same hot ledgers instead of paying repeated DB BLOB reads.

## Trigger

Issue repeated `getTransactions` requests with `format=json` against the newest 1-10 ledgers immediately after they are ingested. Compare the current path to a version that stores raw LCM bytes in the hot-ledger cache and feeds those bytes directly into `processChunksJSON` without calling `BatchGetLedgersBySequences`.

## Target Code

- `cmd/stellar-rpc/internal/daemon/daemon.go:193-197` — daemon creates a hot `LCMCache` specifically for tip-polling `getTransactions` traffic.
- `cmd/stellar-rpc/internal/ingest/lcm_cache.go:13-77` — cache stores only `xdr.LedgerCloseMeta`, not raw `[]byte` ledger blobs.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:409-416` — cache fast path is gated to `request.Format != protocol.FormatJSON`.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:433-469` — JSON mode always fetches DB chunks and then calls `processChunksJSON`.

## Evidence

The daemon comment says the hot-ledger cache exists so tip-polling `getTransactions` requests can bypass SQLite reads, but the current handler only uses it for non-JSON responses. The JSON-optimized path already consumes raw LCM bytes directly via Rust FFI, which means the only thing preventing cache hits is that the cache does not retain the raw bytes needed by that path.

## Anti-Evidence

This only helps requests that stay inside the recent-ledger cache window, and it does not remove the dominant Rust-side JSON extraction cost once the bytes are in hand. If production traffic mostly uses the default XDR format or requests historical ledgers outside the cache window, the win will be limited.

---

## Review

**Verdict**: VIABLE
**Severity**: Medium
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

The LCM cache (`ingest.LCMCache`) stores only typed `xdr.LedgerCloseMeta` values, populated at `service.go:232` after each ingestion commit. The `getTransactions` handler at `get_transactions.go:414` explicitly gates the cache fast path on `request.Format != protocol.FormatJSON` because the JSON path needs raw `[]byte` for `xdr2json.LCMTransactionsToJSON`. This forces every JSON tip-poll to open a SQLite read transaction, execute `BatchGetLedgersBySequences` (which queries `meta` BLOBs and does a partial XDR header decode), and release the snapshot before Rust FFI processing. The inefficiency is real and sits in the hot tip-polling path.

### Code Paths Examined

- `cmd/stellar-rpc/internal/daemon/daemon.go:193-196` — creates `LCMCache` with default size 10 for tip-polling bypass
- `cmd/stellar-rpc/internal/ingest/lcm_cache.go:16-78` — `LCMCache` stores `LedgerBucketWindow[xdr.LedgerCloseMeta]`; `GetAllLCMs` returns typed values only, no raw bytes
- `cmd/stellar-rpc/internal/ingest/service.go:229-235` — cache populated after `tx.Commit` with typed `ledgerCloseMeta` from `ledgerBackend.GetLedger`; raw bytes not available at this point
- `cmd/stellar-rpc/internal/methods/get_transactions.go:409-431` — cache fast path: only entered when `Format != FormatJSON`, releases DB read tx, iterates cached typed LCMs through `processTransactionsInLedger`
- `cmd/stellar-rpc/internal/methods/get_transactions.go:433-471` — fallback path (always used for JSON): calls `BatchGetLedgersBySequences` for raw BLOBs, releases read tx at line 462, then calls `processChunksJSON` with raw bytes
- `cmd/stellar-rpc/internal/db/ledger.go:164-204` — `BatchGetLedgersBySequences` queries SQLite `meta` column, returns `LedgerMetadataChunk{Lcm: []byte, Header: ...}` after partial XDR decode of version + extension + header
- `cmd/stellar-rpc/internal/methods/get_transactions.go:253-336` — `processChunksJSON` uses `chunk.Lcm` (raw bytes) and `chunk.Header` (ledger seq, close time) to drive Rust FFI via `xdr2json.LCMTransactionsToJSON`
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:45-79` — `LCMTransactionsToJSON` pins Go byte slice, passes to Rust `lcm_transactions_to_json` FFI

### Findings

**The inefficiency is confirmed**: The JSON path is explicitly excluded from the cache fast path at line 414. Every JSON tip-poll opens a SQLite read transaction, queries BLOBs, does partial header decode, and releases the snapshot — all of which are avoidable for the most recent 10 ledgers.

**The fix is implementable**: The cache needs to store both the typed `xdr.LedgerCloseMeta` (for the non-JSON path) and the raw `[]byte` (for the JSON path). At ingestion time, raw bytes are not directly available from `ledgerBackend.GetLedger()`, but a single `lcm.MarshalBinary()` call per ingestion (once every ~5 seconds for mainnet) produces the bytes needed. This is negligible overhead.

**Memory cost is modest**: With 10 cached ledgers and typical LCM blobs of 20–100 KB each, the raw byte cache adds 200 KB–1 MB. This is negligible for a server process already holding typed LCMs of similar aggregate size.

**Impact estimate**: For a JSON tip-poll requesting 5 recent ledgers:
- **DB read avoided**: SQLite read tx open + BLOB query + partial XDR header decode ≈ 0.5–3 ms
- **Rust FFI (unchanged)**: LCM parse + JSON serialize ≈ 5–20 ms per ledger
- **Net saving**: ~5–30% of total request latency for cache-eligible requests, depending on ledger transaction density

**Secondary benefit**: Avoiding the SQLite read transaction entirely means one fewer reader holding a WAL snapshot during concurrent ingestion. The code at line 458–461 explicitly documents this WAL checkpoint concern. Eliminating the read transaction for tip-polling reduces checkpoint stall risk under high concurrency.

**Not a duplicate of fail/009**: That hypothesis is about the decode→encode→decode remarshal round-trip within the JSON processing pipeline (Go types → `MarshalBinary` → Rust `read_xdr_to_end`). This hypothesis is about the cache-level bypass — the fact that JSON mode cannot use the hot-ledger cache at all and is forced into the DB read path. These are orthogonal: even if 009's remarshal concern were valid, it would not address the DB read avoidance proposed here.

### PoC Guidance

- **Target code**:
  1. `cmd/stellar-rpc/internal/ingest/lcm_cache.go` — extend `LCMCache` to store a dual representation: both `xdr.LedgerCloseMeta` and `[]byte` (raw XDR). Add a `GetAllRawLCMs(sequences []uint32) ([]db.LedgerMetadataChunk, bool)` method that returns chunks with both raw bytes and the decoded header.
  2. `cmd/stellar-rpc/internal/ingest/service.go:229-235` — after `Append(ledgerCloseMeta)`, also store `lcm.MarshalBinary()` bytes in the cache. Alternatively, modify `Append` to accept both representations.
  3. `cmd/stellar-rpc/internal/methods/get_transactions.go:409-431` — remove the `request.Format != protocol.FormatJSON` gate. Add a JSON-specific cache path that retrieves raw byte chunks and feeds them to `processChunksJSON` instead of `processTransactionsInLedger`.

- **Change description**: Extend `LCMCache` to store raw LCM bytes alongside typed values. At ingestion, call `MarshalBinary()` once per ledger to populate the byte cache. In the handler, allow JSON requests to hit the cache by retrieving raw bytes and constructing `LedgerMetadataChunk` values directly, bypassing `BatchGetLedgersBySequences` entirely.

- **Correctness check**: Existing `getTransactions` tests with `format=json` should pass unchanged — the optimization only changes where bytes come from (cache vs. DB), not what bytes are processed. The `processChunksJSON` function receives identical `LedgerMetadataChunk` data regardless of source.

- **Benchmark focus**: Measure p50/p99 latency of `getTransactions` with `format=json` for the newest 1–5 ledgers under steady-state ingestion. Expect 5–20% p50 improvement for cache-eligible requests, with more pronounced gains under concurrent load (reduced WAL reader contention). Also measure ingestion latency to confirm the per-ledger `MarshalBinary()` cost is negligible (<1ms).
