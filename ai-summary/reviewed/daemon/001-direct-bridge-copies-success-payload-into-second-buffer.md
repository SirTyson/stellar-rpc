# H001: Direct Bridge Copies Marshaled getTransactions Payload into a Second Buffer

**Date**: 2026-04-07
**Subsystem**: daemon
**Severity**: Low
**Impact**: response-copy allocation churn
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

On a successful single-request `getTransactions` call, the daemon should marshal the JSON result once and write it to the client without creating another full-size copy of that result. The JSON-RPC envelope should be added in a way that does not require the already-marshaled page payload to exist twice in memory.

## Mechanism

`directBridge.serveInternal` first builds the full `resultBytes` with `json.Marshal(result)`, then immediately copies those bytes into a second `[]byte` in `directBridgeSuccessResponse`. For large `getTransactions` pages, that produces one extra allocation and one extra memcpy proportional to the response size even after the earlier jhttp bridge round-trip was removed, so the success path still pays avoidable CPU and GC work on every response.

## Trigger

Run `getTransactions` with large pages (`limit=50` or `limit=200`) in either `xdr` or `json` format and compare profiles against a version that writes the JSON-RPC prefix/suffix directly around the marshaled result instead of copying `resultBytes` into `directBridgeSuccessResponse`.

## Target Code

- `cmd/stellar-rpc/internal/directbridge.go:108-123` — marshals the handler result, stores it in `results`, and writes the single-response fast path.
- `cmd/stellar-rpc/internal/directbridge.go:130-141` — allocates a second buffer and appends the full `result` bytes into it.
- `cmd/stellar-rpc/internal/directbridge.go:210-215` — writes only the wrapped buffer to the socket, making the earlier `resultBytes` copy dead after wrapping.

## Evidence

The single-request path still calls `json.Marshal(result)` and then `directBridgeSuccessResponse(parsed.ID, resultBytes)`, whose implementation preallocates a new buffer sized to `len(result)` and appends the full marshaled payload into it. `getTransactions` responses are explicitly called out as large in the direct bridge comment, so this size-proportional copy sits on the method's hot success path rather than on a rare error branch.

## Anti-Evidence

The dominant cost may still be the initial `json.Marshal(result)` call rather than the follow-on copy, so the win is likely bounded to a few percent. If the HTTP duration limiter buffer remains enabled, that outer copy can dwarf this one until the daemon-layer buffering issue is addressed first.

---

## Review

**Verdict**: VIABLE
**Severity**: Informational
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the complete single-request success path through `directBridge.serveInternal`. The handler returns a `protocol.GetTransactionsResponse` Go struct (via `handler.New` wrapping `getTransactionsByLedgerSequence`). Line 108 calls `json.Marshal(result)` which allocates `resultBytes` (~100-200KB for 50 transactions). Line 112 passes `resultBytes` to `directBridgeSuccessResponse`, which allocates a new buffer of size `len(resultBytes)+~40` and copies all result bytes into it via `append(buf, result...)`. For the single non-batch case (line 120-121), `directBridgeWriteJSON` writes only this envelope buffer to `w`, making the original `resultBytes` immediately dead.

### Code Paths Examined

- `cmd/stellar-rpc/internal/directbridge.go:serveInternal:108-112` — confirmed: `json.Marshal(result)` allocates buffer #1, then `directBridgeSuccessResponse` allocates buffer #2 and copies all of buffer #1 into it
- `cmd/stellar-rpc/internal/directbridge.go:directBridgeSuccessResponse:130-141` — confirmed: `make([]byte, 0, len(result)+40)` followed by `append(buf, result...)` performs a full-size allocation and memcpy
- `cmd/stellar-rpc/internal/directbridge.go:directBridgeWriteJSON:211-216` — sets Content-Length from `len(data)` and writes the envelope buffer; the Content-Length dependency is the reason the envelope is materialized rather than streamed
- `cmd/stellar-rpc/internal/directbridge.go:serveInternal:120-121` — single non-batch fast path writes `results[0]` directly; this is the path where the optimization applies
- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:275-307` — returns `protocol.GetTransactionsResponse` struct; `json.Marshal` on this struct is the dominant serialization cost
- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:77-235` — builds `[]protocol.TransactionInfo` with XDR/JSON fields; the struct contains large string fields (base64 XDR or raw JSON) that produce the bulk of the marshaled output

### Findings

**The inefficiency is real.** For a single non-batch getTransactions request returning 50 transactions (~200KB response), `directBridgeSuccessResponse` allocates ~200KB and performs a ~200KB memcpy that is entirely avoidable. The original `resultBytes` becomes garbage immediately after the copy.

**The fix is correct.** For the single-request case, the envelope can be written in parts directly to `w` without materializing the full envelope buffer: compute Content-Length as `len(prefix) + len(idJSON) + len(middle) + len(resultBytes) + 1`, set the header, then write the prefix, resultBytes, and closing brace separately. Go's `net/http` ResponseWriter uses a `bufio.Writer` internally, so multiple small writes coalesce into a single TCP segment. The batch path (line 122-124) would continue using the current approach since `directBridgeJoinBatch` needs complete `json.RawMessage` items.

**The impact is sub-1% of getTransactions latency.** A 200KB `memcpy` costs ~20-50μs on modern hardware. Against total getTransactions latency of 5-50ms (dominated by DB reads, XDR parsing, and `json.Marshal` reflection/serialization), the copy represents ~0.1-1%. The `make([]byte, 200KB)` allocation adds minor GC pressure — at 1000 RPS that's ~200MB/s of extra allocation, but Go's GC handles large pointer-free slices efficiently. The improvement would show in `alloc_objects` and `alloc_space` pprof profiles but is unlikely to produce a measurable latency reduction in benchmarks.

**Severity downgrade rationale:** The hypothesis claimed Low severity (<5% but measurable). The actual impact is likely sub-1% and may not be visible in latency benchmarks, only in allocation profiles. This meets the Informational threshold (real finding, minimal direct impact). The related reviewed H001 (HTTP duration limiter double-buffer) addresses a larger copy in the same response path; if that optimization lands first, the relative impact of this copy becomes even smaller.

**Interaction with H001 (bufferedResponseWriter):** If the HTTP duration limiter's `bufferedResponseWriter` is still active, the response path includes an additional copy layer on top of this one. Eliminating both copies (H001 + this) would have compounding benefits, but each independently has sub-5% impact.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/directbridge.go:serveInternal` lines 108-124 — add a fast path for single non-batch success that writes envelope parts directly to `w` instead of building the envelope buffer. Keep the existing `directBridgeSuccessResponse` for the batch path.
- **Change description**: After `json.Marshal(result)` on line 108, detect the single non-batch success case (can be tracked via a counter or checked after the loop). For that case, compute the Content-Length arithmetically, set headers, and call `w.Write` for the prefix, `resultBytes`, and suffix separately, bypassing `directBridgeSuccessResponse` and the `results` slice entirely.
- **Correctness check**: `make go-test` — the existing directbridge tests and integration tests cover single-request, batch, error, and notification paths. Verify that responses remain byte-identical (same JSON-RPC envelope structure).
- **Benchmark focus**: Measure `alloc_objects` and `alloc_space` via pprof for getTransactions with limit=200, format=xdr. The ~200KB envelope allocation should disappear. Latency improvement is expected to be sub-1%, so focus on allocation metrics rather than latency p50/p99.
