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
