# H003: Default getTransactions Builds Large Base64 Strings Before the Bridge Can Write JSON

**Date**: 2026-04-07
**Subsystem**: daemon
**Severity**: Medium
**Impact**: allocation churn / XDR-mode latency
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

For the default XDR/base64 response format, `getTransactions` should encode XDR fields directly into the outgoing JSON payload using reusable buffers. It should not first allocate immutable Go strings for every envelope, result, meta, and event only for `directBridge` to walk the struct and copy those same bytes into a second response buffer moments later.

## Mechanism

The XDR path fills `protocol.TransactionInfo` with `string` and `[]string` fields by repeatedly calling `EncodingBuffer.MarshalBase64`. That helper uses `UnsafeMarshalBase64` and then does `return string(b)`, forcing a copy from the reusable scratch buffer into a new heap string for every field and event. `getTransactionsByLedgerSequence` retains those strings in the `txns` slice until `directBridge` later calls `json.Marshal(result)`, which copies them again into the final JSON response bytes. A getTransactions-specific writer that appends quoted output from `UnsafeMarshalBase64` directly into the response buffer could remove the intermediate string copies and reduce peak heap size for large default-format pages.

## Trigger

Benchmark the default `getTransactions` format (`format=""` / base64) with `limit=200`, especially on ledgers carrying many diagnostic or contract events, then compare allocation profiles and tail latency against a path that writes base64 directly from `UnsafeMarshalBase64` into a specialized `getTransactions` encoder instead of populating `TransactionInfo` string fields.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:188-227` — allocates base64 strings for result/meta/envelope plus per-event string slices before appending to `txns`.
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/xdr/main.go:UnsafeMarshalBase64/MarshalBase64:197-205,263-269` — reusable scratch buffer exists, but `MarshalBase64` copies it into a new `string`.
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/protocols/rpc/get_transactions.go:42-90` — the API shape stores the encoded payload as strings inside a large response struct.
- `cmd/stellar-rpc/internal/directbridge.go:serveInternal:108-123` — later marshals the entire populated response struct into a separate JSON byte buffer.

## Evidence

The SDK helper explicitly shows the extra copy: `MarshalBase64` calls `UnsafeMarshalBase64` and then converts the returned `[]byte` to `string`. The handler then stores many of those strings in `txns`, while `directBridge` always performs a later full `json.Marshal(result)` pass over the same response content.

## Anti-Evidence

Base64 encoding itself is still required, so this does not eliminate the core encode cost the way a raw-byte fast path would. It also overlaps with broader bridge-side encoder work, so the measurable win depends on how much of the page's total time is currently spent on string allocation and GC rather than on XDR marshaling itself.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated (related but distinct from reviewed H002 on generic json.Marshal reflective path)

### Trace Summary

Traced the complete XDR/base64 path from `processTransactionsInLedger` through `MarshalBase64` to `directBridge.serveInternal`. Confirmed that each call to `enc.MarshalBase64()` invokes `UnsafeMarshalBase64` (which base64-encodes into a reusable `scratchBuf`), then performs `return string(b), nil` which allocates a new heap string and copies the base64 bytes. For 200 transactions, this produces ~1000+ string allocations (3 per tx for envelope/result/meta, plus N per tx for diagnostic events and contract events). These strings are held live in the `txns` slice until `directBridge` calls `json.Marshal(result)` at `directbridge.go:108`, which copies them a second time into the JSON output via `appendString`.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:189-221` — confirmed: default path calls `enc.MarshalBase64()` for `ResultXDR`, `ResultMetaXDR`, `EnvelopeXDR`, each `DiagnosticEventsXDR[j]`, and each event field in `buildEventsXDRDirect`. Each call returns a new heap string.
- `go-stellar-sdk@v0.4.0/xdr/main.go:UnsafeMarshalBase64:197-205` — confirmed: base64 encodes into `e.scratchBuf` (a reusable buffer grown via `growSlice`), returns the scratch buffer slice. The scratch buffer is overwritten on every call.
- `go-stellar-sdk@v0.4.0/xdr/main.go:MarshalBase64:263-269` — confirmed: calls `UnsafeMarshalBase64(encodable)` then `return string(b), nil`. The `string(b)` conversion forces an allocation + memcpy because Go strings are immutable and the scratch buffer will be reused.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:buildEventsXDRDirect:237-266` — confirmed: same pattern for `TransactionEventsXDR` and `ContractEventsXDR` — each event is individually `MarshalBase64`'d into a string.
- `cmd/stellar-rpc/internal/directbridge.go:serveInternal:108` — confirmed: `json.Marshal(result)` on the complete `GetTransactionsResponse` struct encodes all string fields via `appendString`, which for ASCII-safe base64 is essentially a second memcpy with trivial per-byte escaping checks.
- `go-stellar-sdk@v0.4.0/protocols/rpc/get_transactions.go:42-90` — confirmed: `TransactionDetails` stores XDR payloads as `string` and `[]string` fields; `Events` struct adds `[]string` and `[][]string` fields. All populated in the hot loop.

### Findings

**The inefficiency is real.** Every base64 field goes through two full-size copies: (1) `string(b)` in `MarshalBase64` copies from the scratch buffer to a new heap string, (2) `json.Marshal` copies each string into the JSON output buffer via `appendString`. The scratch buffer reuse design in `EncodingBuffer` makes copy (1) unavoidable with the current architecture — you cannot hold a reference to the scratch buffer because it's overwritten by the next `MarshalBase64` call.

**Data volume per request.** For 200 Soroban transactions (worst case): `ResultMetaXDR` ~14-270KB base64 each (meta dominates), `EnvelopeXDR` ~3-14KB, `ResultXDR` ~0.3-1.3KB, plus diagnostic and contract events. Total base64 data per page: ~4-60MB. The `string(b)` copies for this data cost ~0.2-3ms (at ~20GB/s memcpy). The subsequent `json.Marshal` `appendString` encoding costs an additional ~1-10ms (at ~3-5GB/s for per-byte escape checks). Combined waste: ~1-13ms for Soroban-heavy pages.

For typical payment transactions (200 txs × ~3KB each = ~600KB total), copy costs are <0.1ms — negligible.

**GC pressure is the secondary concern.** The ~1000+ intermediate string allocations (ranging from 300 bytes to 270KB each) create GC pressure. At high concurrency (e.g., 50 concurrent getTransactions requests), this could mean 200MB-3GB of live intermediate strings, increasing GC pause frequency and CPU overhead.

**Architectural constraint.** The fix CANNOT be implemented by simply changing `MarshalBase64` to return `[]byte` — the scratch buffer is reused, so callers would hold dangling references. The only correct approach is to consume the base64 bytes immediately (before the next `MarshalBase64` call) by writing them directly into a JSON output buffer. This requires a streaming JSON encoder that integrates with the XDR encoding loop.

**Overlap with reviewed H002.** The already-reviewed hypothesis `002-generic-json-marshal-keeps-gettransactions-on-reflective-path` addresses the `json.Marshal` side of this problem (the `appendCompact`/`appendString` overhead). H003 addresses the upstream side (the `string(b)` copies). Together they form a complete streaming pipeline optimization. H003 cannot be implemented independently of H002 — the handler must produce pre-built JSON (eliminating intermediate strings), and the bridge must accept pre-built JSON (eliminating re-serialization). The PoC should implement both together.

**Severity assessment.** The hypothesis claims Medium (5-20%). For Soroban-heavy pages, the combined string copy + re-encode waste is ~1-13ms against total latency of ~10-50ms (5-25%). However, this is the combined H002+H003 improvement. H003's contribution alone (the `string(b)` copies) is ~0.2-3ms, which is ~1-6% of total latency — solidly Low. For typical payment transactions, savings are negligible. Downgraded to Low.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger` (default branch, lines 189-221) and `cmd/stellar-rpc/internal/directbridge.go:serveInternal:108`. This optimization should be co-implemented with reviewed H002's custom encoder.
- **Change description**: Add a `marshalGetTransactionsXDR(enc *xdr.EncodingBuffer, txns []txnData, ...) json.RawMessage` function that builds the complete JSON response incrementally. For each XDR field, call `enc.UnsafeMarshalBase64()` and immediately `append(buf, '"')` + `append(buf, b64bytes...)` + `append(buf, '"')` into the output buffer, bypassing both the `string(b)` copy and the later `json.Marshal` encoding. The handler returns `json.RawMessage` instead of `protocol.GetTransactionsResponse`. In directBridge, add a type-switch for `json.RawMessage` results that skips `json.Marshal` entirely (avoids `appendCompact` overhead from H002).
- **Correctness check**: `make go-test` — existing `TestGetTransactions` and integration tests cover both XDR and JSON format responses. The custom encoder must produce JSON byte-identical to `json.Marshal(protocol.GetTransactionsResponse{...})` for all field combinations. Pay special attention to `omitempty` semantics for empty event slices.
- **Benchmark focus**: Compare `alloc_objects`, `alloc_space`, and wall-clock latency for `getTransactions` with `limit=200` on Soroban-heavy ledgers. Target: eliminate ~1000+ intermediate string allocations per request, reduce peak heap by ~50% for large pages, and improve latency by ~1-3ms (Soroban) or ~0.1ms (payments). Use `pprof` heap profiles to measure allocation reduction.
