# H001: Concurrent identical pollers recompute the same `getTransactions` page in parallel

**Date**: 2026-04-07
**Subsystem**: daemon
**Severity**: Medium
**Impact**: concurrent duplicate CPU / allocation fan-out
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When many clients ask for the exact same recent `getTransactions` page at the same time, the daemon should compute that page once and fan the result out to all waiters. Concurrent pollers for the same `(startLedger, cursor, limit, format)` against the same latest ledger should not all repeat the planner query, hot-ledger lookup, serialization, and bridge marshaling independently.

## Mechanism

`directBridge` dispatches each HTTP request independently, and `transactionsRPCHandler` has no in-flight deduplication state beyond the per-ledger envelope cache. That means a burst of identical tip pollers all run `getTransactionsByLedgerSequence()` separately, even though the returned `protocol.GetTransactionsResponse` is fully determined by the request parameters plus the current `ledgerRange`. A `singleflight.Group` keyed by the request shape plus `ledgerRange.LastLedger.Sequence` / `CloseTime` could collapse those overlapping calls so only one request does the expensive work.

## Trigger

1. Let a new ledger ingest.
2. Fire 10-100 concurrent `getTransactions` requests with the same default latest-page shape (same `startLedger`, no cursor, same `limit`, same `format`).
3. Compare CPU, allocations, and tail latency against a build that wraps `getTransactionsByLedgerSequence()` in a request-keyed singleflight and shares the computed result with waiters.

## Target Code

- `cmd/stellar-rpc/internal/directbridge.go:58-125` — each HTTP request dispatches its handler call independently; there is no bridge-level request coalescing.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:25-35` — `transactionsRPCHandler` stores only readers, limits, and `envCache`; there is no in-flight dedupe state.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:350-522` — every request rebuilds the page from planner lookup through format-specific assembly.
- `cmd/stellar-rpc/internal/ingest/service.go:229-235` — new ledgers enter the hot path at a clear invalidation boundary the coalescing key can incorporate.

## Evidence

The handler has no memoization or request-collapsing field; every call allocates a fresh `xdr.EncodingBuffer`, runs `GetLedgerSequencesWithTransactions`, and then either enters `processChunksJSON()` or `processTransactionsInLedger()`. `directBridge` invokes the handler inline per request, so identical pollers can multiply the same page work N times during the busiest post-close fan-out window.

## Anti-Evidence

This helps only when identical requests overlap in time; staggered or slightly different poll shapes still miss. The implementation also has to share only the computed result body, not the full JSON-RPC envelope, because each caller has its own `id`.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full request path from `directBridge.ServeHTTP` → `serveInternal` (line 58-125) → handler dispatch (line 93) → `getTransactionsByLedgerSequence` (line 350-522). Confirmed that each concurrent request independently opens a SQLite read transaction (line 353), queries the transaction index (line 403), fetches LCM data (line 440 for JSON, line 476 for XDR streaming), performs format-specific processing (Rust FFI for JSON, XDR encoding for base64), and marshals the response. No singleflight, memoization, or in-flight deduplication exists anywhere in the chain. The LCM cache (line 418) partially mitigates duplicate DB reads for XDR format only — JSON format always hits SQLite + Rust FFI.

### Code Paths Examined

- `cmd/stellar-rpc/internal/directbridge.go:58-125` — `serveInternal` dispatches each request independently via `h(req.Context(), jrequest)` at line 93; no request-level batching or coalescing
- `cmd/stellar-rpc/internal/methods/get_transactions.go:25-35` — `transactionsRPCHandler` struct has no singleflight or dedup field; `envCache` only caches XDR envelope deserialization, not full responses
- `cmd/stellar-rpc/internal/methods/get_transactions.go:350-522` — `getTransactionsByLedgerSequence` opens a fresh read transaction, queries index, fetches LCMs, processes format-specific output; all work is per-request
- `cmd/stellar-rpc/internal/methods/get_transactions.go:525-543` — `NewGetTransactionsHandler` wraps the method directly via `handler.New`; no middleware or caching layer
- `cmd/stellar-rpc/internal/ingest/lcm_cache.go:16-78` — `LCMCache` uses RWMutex-guarded circular buffer; only serves XDR path (JSON explicitly excluded at line 418 of get_transactions.go)

### Findings

1. **The inefficiency exists.** Every concurrent identical request independently: (a) opens a SQLite read transaction, (b) queries the transaction index, (c) batch-fetches or streams LCMs from SQLite, (d) processes transactions through format-specific paths (Rust FFI for JSON, XDR encoding for base64), and (e) marshals the response. None of these steps are deduplicated across concurrent identical requests.

2. **JSON path is disproportionately affected.** The LCM cache (`lcmCache.GetAllLCMs`) explicitly excludes JSON format (line 418: `request.Format != protocol.FormatJSON`), so JSON tip pollers always hit SQLite + Rust FFI. The Rust FFI work (`processChunksJSON` at line 468, involving 6+ CGo boundary crossings per page) is the most expensive per-request operation and the best candidate for deduplication.

3. **The response is deterministic for the same inputs + ledger state.** `GetTransactionsResponse` is fully determined by `(startLedger, cursor, limit, format, ledgerRange)`. The `ledgerRange` changes only on ingestion, making it a natural invalidation boundary. The `json.Marshal` at directBridge line 108 is read-only, so sharing the struct across goroutines is safe.

4. **Implementation requires careful key construction.** The singleflight key must include `ledgerRange.LastLedger.Sequence` (read at line 364), which means the cheap DB read for ledger range would still execute per-request. The singleflight would wrap lines 380-513 (the expensive pagination + fetch + processing + conversion). This is acceptable — the ledger range query is a single indexed SQLite read, negligible compared to the batch LCM fetch and FFI processing.

5. **Context cancellation needs handling.** Go's `singleflight.Do` runs the winner's function; if the winning caller's HTTP context is cancelled, all waiters fail. The implementation must use a detached context (e.g., `context.WithoutCancel`) for the shared computation, with each waiter independently checking its own context after receiving the shared result.

6. **Impact is workload-dependent.** The benefit scales with the number of concurrent identical requests. Tip-polling (exchanges, explorers polling latest ledger) is the primary scenario. At moderate concurrency (10-50 pollers per server), the overlap window during a single request's processing time (10-50ms) would typically see 1-5 concurrent identical requests. The deduplication would save proportionally on CPU and allocation, with the JSON path seeing the largest per-request savings.

### PoC Guidance

- **Target code**: Add a `singleflight.Group` field to `transactionsRPCHandler` (get_transactions.go line 25-35). Wrap lines 380-513 of `getTransactionsByLedgerSequence` in a `singleflight.Do` call keyed on `fmt.Sprintf("%d:%s:%d:%s:%d", request.StartLedger, cursor, limit, request.Format, ledgerRange.LastLedger.Sequence)`.
- **Change description**: After reading `ledgerRange` (line 364) and validating the request (line 372), construct the singleflight key and wrap the remaining expensive work. Use `context.WithoutCancel(ctx)` inside the singleflight function to prevent one caller's cancellation from failing all waiters. Each waiter checks `ctx.Err()` after receiving the shared result.
- **Correctness check**: Existing `TestGetTransactions*` tests cover the handler; ensure concurrent tests don't regress. The shared `GetTransactionsResponse` must not be mutated after return — verify no post-processing modifies the struct (currently none does: line 515 returns it directly).
- **Benchmark focus**: Create a targeted benchmark firing N concurrent identical requests (same startLedger, no cursor, default limit, JSON format) against a recently ingested ledger. Measure: (1) total CPU time reduction, (2) per-request p99 latency, (3) allocation reduction via `testing.B` memory stats. At N=50 concurrent identical JSON requests, expect ~40-80% reduction in total CPU for those requests. Overall RPS improvement in a mixed workload is likely <5%.
