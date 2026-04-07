# H003: Batched `getTransactions` replies accumulate and recopy the entire payload before write-out

**Date**: 2026-04-07
**Subsystem**: network
**Severity**: Medium
**Impact**: batch heap pressure / copy amplification
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

A JSON-RPC batch containing multiple large `getTransactions` calls should not need to keep every member response envelope live and then duplicate all of them into one giant aggregate slice before the HTTP timeout layer copies the batch again. Batch assembly should avoid a full second pass over the total batch payload.

## Mechanism

For batch requests, `serveInternal` appends each fully materialized member response to `results`, and `directBridgeJoinBatch` then allocates a new aggregate buffer and copies every `results[i]` into `[ ... ]`. With batched JSON-format `getTransactions`, each member can already be megabytes long, so transient live bytes become `sum(member envelopes)` plus the joined batch plus the HTTP timeout buffer. That copy amplification can turn one batched request into a large GC event and materially increase tail latency or reduce sustainable RPS for batch clients.

## Trigger

Send JSON-RPC batch requests containing 4-16 `getTransactions` calls with `xdrFormat=json` and `pagination.limit=200`, and compare baseline against a version that streams batch assembly or constructs the final batch buffer once without retaining and re-copying each full member envelope.

## Target Code

- `cmd/stellar-rpc/internal/directbridge.go:69-123` — `serveInternal` retains every member response in `results`
- `cmd/stellar-rpc/internal/directbridge.go:189-207` — `directBridgeJoinBatch` allocates a second buffer and copies all member envelopes into it
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:88-90` — the HTTP timeout wrapper adds another full copy of the joined batch
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/protocols/rpc/get_transactions.go:74-90` — each batch member can contain many large raw JSON transaction fields

## Evidence

The current batch path explicitly stores each member as a separate `json.RawMessage` and then concatenates those messages into a new array buffer. Because `getTransactions` responses are large, the extra aggregate copy is proportional to the total batch payload, not to a small metadata header.

## Anti-Evidence

Single-request `getTransactions` traffic sees no benefit from this optimization, so the impact depends on clients actually using JSON-RPC batch. If production callers almost never batch `getTransactions`, this may have little real-world effect despite being mechanically valid.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated (complementary to reviewed 002-direct-bridge-double-buffers which targets per-member `resultBytes → envelope` copy; this targets the batch-level `envelopes → joinedBatch` copy, a distinct step in the copy chain)

### Trace Summary

Traced the full batch path through `directBridge.serveInternal` (directbridge.go:58-126). For batch JSON-RPC requests, `serveInternal` accumulates N individual `json.RawMessage` envelope slices in `results` (line 70-112), then `directBridgeJoinBatch` (line 190-208) pre-computes exact size and copies all N envelopes into one contiguous buffer, then `directBridgeWriteJSON` (line 211-216) writes the joined buffer to the `http.ResponseWriter` which under the timeout wrapper hits `bufferedResponseWriter.Write` (requestdurationlimiter.go:88-90) causing another full-payload copy. At the moment of the final write, three full-payload-sized buffers are simultaneously live: the individual envelopes in `results`, the joined batch from `directBridgeJoinBatch`, and the `bufferedResponseWriter.buffer` — approximately 3× the total batch payload.

### Code Paths Examined

- `cmd/stellar-rpc/internal/directbridge.go:69-70` — `isBatch` flag and `results` accumulator: `var results []json.RawMessage` starts the per-member accumulation
- `cmd/stellar-rpc/internal/directbridge.go:108-112` — for each successful member: `json.Marshal(result)` → `resultBytes`, then `directBridgeSuccessResponse(id, resultBytes)` → individual envelope (~5MB each for large getTransactions), appended to `results`
- `cmd/stellar-rpc/internal/directbridge.go:120-124` — batch vs single dispatch: single request writes `results[0]` directly (line 121, no join needed), batch calls `directBridgeJoinBatch(results)` (line 123)
- `cmd/stellar-rpc/internal/directbridge.go:190-208` — `directBridgeJoinBatch`: pre-computes exact buffer size from all members (lines 191-197), allocates one buffer with `make([]byte, 0, size)` (line 198), copies all members via `append(buf, r...)` (line 204). One allocation + one full-payload memcpy. The individual `results` members remain live on the caller's stack.
- `cmd/stellar-rpc/internal/directbridge.go:211-216` — `directBridgeWriteJSON`: sets Content-Type, Content-Length, status, then calls `w.Write(data)`. Under the timeout wrapper, `w` is a `*bufferedResponseWriter`.
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:88-90` — `bufferedResponseWriter.Write`: `w.buffer = append(w.buffer, buf...)` copies the entire joined batch into a new buffer. This is architecturally required by the timeout mechanism (fail 004, fail 015 already investigated).
- `cmd/stellar-rpc/internal/jsonrpc.go:332-336,359-365` — wiring: `directBridge` → `httpRequestDurationLimiter` (with `bufferedResponseWriter`) → `BacklogHTTPQLimiter`. Confirmed all HTTP traffic uses this stack.

### Findings

The batch copy chain for a JSON-RPC batch of N `getTransactions` calls is:

1. **Per member**: `json.Marshal(result)` → `resultBytes` (~5MB alloc + copy), then `directBridgeSuccessResponse` → `envelope` (~5MB alloc + copy). The `resultBytes → envelope` copy is targeted by the independent reviewed hypothesis 002-direct-bridge-double-buffers.
2. **Batch join**: `directBridgeJoinBatch(results)` → one contiguous `joinedBatch` buffer (~N×5MB alloc + N×5MB memcpy). **This is the copy targeted by this hypothesis.**
3. **Timeout buffer**: `bufferedResponseWriter.Write(joinedBatch)` → `w.buffer` (~N×5MB alloc + N×5MB copy). This copy is architecturally required and has already been investigated (fail 004: sync.Pool for buffer, fail 024: Content-Length preallocation — both not viable).

**Peak memory analysis (N=4 members × 5MB each = 20MB batch):**
- Current: `results` (20MB) + `joinedBatch` (20MB) + `bufferedResponseWriter.buffer` (20MB) = **~60MB peak**
- Optimized (incremental batch building): `batchBuf` (20MB, built incrementally) + `bufferedResponseWriter.buffer` (20MB) = **~40MB peak**
- Savings: **~20MB (33%) peak memory reduction** per batch request

**For larger batches (N=16 × 5MB = 80MB):** Peak drops from ~240MB to ~160MB, saving ~80MB.

**Latency analysis:**
- Eliminated memcpy: 20-80MB at ~20 GB/s = 1-4ms
- Total batch request time: N × (50-5000ms handler + 5-10ms marshal) = 200-80,000ms
- Latency savings: 0.005-2% — below the ±30% futurenet benchmark noise floor

**The primary value is GC pressure reduction, not latency.** Eliminating one 20-80MB transient allocation per batch request directly reduces GC marking and sweeping work. This is measurable via `-benchmem` even when latency effects are in the noise.

**Severity downgrade from Medium to Low:** The hypothesis claims 5-20% latency reduction, but actual latency savings are <2%. The real benefit (GC pressure / peak memory reduction) is measurable but does not translate to a 5%+ latency or RPS gain. Additionally, the optimization only helps batch requests — single requests already bypass `directBridgeJoinBatch` (line 120-121).

**Independence from reviewed 002:** This optimization and reviewed 002-direct-bridge-double-buffers are complementary. 002 eliminates per-member `resultBytes → envelope` copy (step 1 above). This hypothesis eliminates the batch `envelopes → joinedBatch` copy (step 2). Applying both would reduce peak memory from 3× to ~1.5× batch payload.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/directbridge.go` — refactor `serveInternal` (lines 69-124) to build batch responses incrementally into a single `[]byte` buffer instead of accumulating individual `results` and joining via `directBridgeJoinBatch`
- **Change description**: For batch requests, replace the `results []json.RawMessage` accumulation pattern with a single growing `batchBuf []byte`. Write `[` at the start of the loop, append each member envelope directly into `batchBuf` (with comma separators), and write `]` after the loop. Pre-compute and track the content-length for the header. For error/notification members, append their JSON directly to `batchBuf` as well. Then call `directBridgeWriteJSON(w, http.StatusOK, batchBuf)` once. The single-request path (line 120-121) is unaffected. Note: `batchBuf` grows via `append` so it may reallocate during the build phase; this is still better than the current approach because it avoids retaining N separate envelope slices simultaneously.
- **Correctness check**: The existing tests in `cmd/stellar-rpc/internal/directbridge_test.go` (if present) and integration tests that exercise batch JSON-RPC requests. Verify JSON output byte-for-byte equivalence for batch responses with: (a) all success members, (b) mixed success/error members, (c) notifications (no response), (d) single-member batches. The `isBatch` flag (line 69) must still be respected for single-element batches (`[{...}]` must return `[...]` not `...`).
- **Benchmark focus**: Measure with `-benchmem` for batch `getTransactions` requests (4-16 members, JSON format, limit=200). Key metrics: (1) allocations per batch request — expect one fewer large allocation, (2) total bytes allocated — expect ~20-80MB reduction per batch, (3) peak RSS during batch processing. Latency improvement may be below noise floor — allocation metrics are the reliable signal.
