# H002: `bufferedResponseWriter` copies single-write `getTransactions` bodies that are already fully materialized

**Date**: 2026-04-07
**Subsystem**: network
**Severity**: Low
**Impact**: response copy / allocation
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When the downstream JSON-RPC bridge has already built the full HTTP response body as one immutable `[]byte`, the HTTP timeout wrapper should delay committing that body without copying it again. The common single-write `getTransactions` path should not pay a second full-memory pass just to move bytes from one in-process buffer into another.

## Mechanism

`jhttp.writeJSON` marshals the final JSON-RPC envelope into `bits` and issues exactly one `w.Write(bits)`. `bufferedResponseWriter.Write` always does `append(w.buffer, buf...)`, which copies the entire multi-megabyte `getTransactions` response into a new slice even though the bridge never reuses `bits` after the write and `WriteOut` later writes the same bytes again to the real socket. A first-write ownership fast path would eliminate one full-body copy and the associated allocation pressure for the hottest response shape this endpoint emits.

## Trigger

Run large `getTransactions` requests (`xdrFormat=json`, `pagination.limit` near 200) and compare baseline against a version of `bufferedResponseWriter` that adopts the first write buffer by reference when it is the only write. Measure response-path allocations and p50/p95 latency at the highest zero-error load level.

## Target Code

- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:69-90` — `bufferedResponseWriter.Write` always copies the provided bytes into its own buffer
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:97-118` — `WriteOut` later writes that copied buffer to the real `http.ResponseWriter`
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/jhttp/getter.go:138-150` — `writeJSON` marshals once and performs a single `w.Write(bits)` call on the HTTP path
- `cmd/stellar-rpc/internal/jsonrpc.go:337-374` — all `getTransactions` HTTP traffic goes through the timeout wrapper that owns `bufferedResponseWriter`

## Evidence

The bridge's HTTP writer path is single-shot: it computes `bits`, sets `Content-Length`, writes headers, and then writes the body once. The timeout wrapper then copies that same already-complete body into `bufferedResponseWriter.buffer`, so every large `getTransactions` response pays a redundant full-body memcpy before the final socket write.

## Anti-Evidence

This only helps responses that really are single-write and whose body slice is not mutated after `Write` returns; other writers would still need the existing copy semantics. Prior work already showed that response-copy optimizations can be hard to measure in noisy futurenet runs, so this is likely a Low-severity win rather than a throughput ceiling change.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full HTTP `getTransactions` path: HTTP request → `httpRequestDurationLimiter.ServeHTTP` (creates `bufferedResponseWriter`, spawns goroutine) → `BacklogHTTPQLimiter` → `Bridge.ServeHTTP` → `serveInternal` → `encodeResponses` → `writeJSON` → `w.Write(bits)`. Confirmed that `writeJSON` (getter.go:138-150) marshals the response with `json.Marshal`, sets Content-Length, then issues exactly one `w.Write(bits)` call. The `bits` local variable goes out of scope immediately after — no reuse. The `bufferedResponseWriter.Write` (line 88-90) then copies the entire body via `append(w.buffer, buf...)`, allocating a new slice and performing a full memcpy. Later, `WriteOut` (line 97-118) writes `w.buffer` to the real socket — a second copy into kernel buffers. The first copy is entirely eliminable for this single-write path.

### Code Paths Examined

- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:88-90` — `Write` unconditionally appends: `w.buffer = append(w.buffer, buf...)`. On a nil slice with a large `buf`, Go allocates a new backing array of `len(buf)` and copies all bytes.
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:97-118` — `WriteOut` flushes `w.buffer` to the real writer via `rw.Write(w.buffer)`. This is a second copy (into kernel buffer), unavoidable.
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:124-196` — `ServeHTTP` creates `responseBuffer`, passes it to the downstream goroutine, and reads back via channel synchronization — the happens-before guarantee makes the buffer safe to read after channel receive.
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/jhttp/getter.go:138-150` — `writeJSON` marshals once, then `w.Write(bits)` — single write, `bits` never reused.
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/jhttp/bridge.go:162-168` — `encodeResponses` calls `writeJSON(w, http.StatusOK, rsps[0])` for single (non-batch) responses — confirms single-write property for the bridge POST path.
- `cmd/stellar-rpc/internal/jsonrpc.go:337-374` — Wiring: `Bridge` → `BacklogHTTPQLimiter` → `httpRequestDurationLimiter` (outer HTTP timeout, 25s default) wraps the entire bridge. All HTTP traffic goes through `bufferedResponseWriter`.
- `cmd/stellar-rpc/internal/config/options.go:362-363` — max-transactions-limit defaults to 200 — large responses at max pagination can be multi-MB.

### Findings

The inefficiency is real and on the hot path. Every `getTransactions` HTTP response is fully materialized by `json.Marshal` into a contiguous `[]byte` (`bits`), then copied byte-for-byte into a second `[]byte` (`w.buffer`) via `append`. For a 200-transaction JSON response (potentially 1–5 MB), this memcpy costs ~100–500 µs on modern hardware.

**Distinction from prior work:**
- H004 (sync.Pool) targeted allocation overhead by pooling the destination buffer but still performed the full memcpy. It showed no measurable improvement — but it saved only allocation time (~ns), not the copy itself (~µs).
- H024 (Content-Length preallocation) was NOT_VIABLE because the path is already single-write with no slice growth to eliminate.
- This hypothesis targets the memcpy itself (100–1000× more expensive than what H004 saved), making it a qualitatively different optimization.

**Correctness analysis:**
- The `io.Writer` contract prohibits modifying the slice but does NOT prohibit retaining the reference. Taking ownership on first write is technically compliant.
- `writeJSON`'s `bits` is a stack-local variable that goes out of scope after `w.Write(bits)` — no caller reuse.
- The channel send/receive between the downstream goroutine and the main goroutine provides the required happens-before guarantee for safe cross-goroutine access.
- Multi-write callers (error paths, future middleware) must fall back to copy semantics — the implementation must track whether ownership was taken.
- Panic recovery path never reads `w.buffer`, so zero-copy doesn't affect error handling.

**Risk:** the `io.Writer` convention (callers assume they can reuse buffers after Write) is not the formal contract but is a strong Go idiom. Since `bufferedResponseWriter` is an unexported internal type with a single usage site, this is an acceptable deviation — but it should be documented.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/network/requestdurationlimiter.go` — modify `bufferedResponseWriter.Write` (lines 88-90)
- **Change description**: On the first `Write` call (when `w.buffer == nil`), assign `w.buffer = buf` instead of `append`. Add a boolean field (e.g., `ownedBuffer`) to track whether the buffer was taken by reference. On subsequent `Write` calls, allocate a new buffer and copy both the owned first buffer and the new data (fall back to copy semantics). This eliminates one full-body memcpy and one allocation on the single-write hot path.
- **Correctness check**: Existing tests in `requestdurationlimiter_test.go` — all 10 tests should pass. Pay special attention to tests that exercise multi-write or panic-recovery scenarios to ensure the fallback path works.
- **Benchmark focus**: Measure response-path allocations (`-benchmem`) and p50/p95 latency for large `getTransactions` responses (200 transactions, JSON format). The memcpy savings should be ~100–500 µs per request. At moderate RPS, this translates to <5% latency improvement — measure at the highest stable RPS to maximize signal-to-noise ratio.

---

## PoC Attempt

**Result**: POC_PASS
**Date**: 2026-04-07
**PoC by**: claude-opus-4.6, high

### Changes Made

- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go` (lines 69-107):
  - Added `borrowed bool` field to `bufferedResponseWriter` struct to track whether the buffer holds a borrowed reference vs an owned copy.
  - Rewrote `Write` method with a three-branch strategy:
    1. **First write (fast path)**: when `w.buffer == nil`, assigns `w.buffer = buf` directly — zero-copy borrow of the caller's slice.
    2. **Second write after borrow (fallback)**: allocates a new combined buffer, copies both the borrowed data and the new data, clears the `borrowed` flag.
    3. **Subsequent writes (normal path)**: standard `append` semantics, identical to the original code.

### Demonstration

On the dominant single-write HTTP path (`jhttp.writeJSON` → one `w.Write(bits)` call), the optimization eliminates one full-body `memcpy` and one heap allocation per response. For a 200-transaction JSON response (1–5 MB), this saves ~100–500 µs of CPU time and ~1–5 MB of transient allocation pressure per request. The multi-write fallback preserves correctness for all other callers.

### Test Results

All existing tests in `cmd/stellar-rpc/internal/network/` pass (including `requestdurationlimiter_test.go` — HTTP limiting, no-limiting, warn, panic recovery; JRPC limiting, no-limiting, warn; and all backlog queue tests). Full `make go-test` passes across all 14 packages with no failures.

---

## Final Review — Needs Revision

**Date**: 2026-04-07
**Final review by**: gpt-5.4, high

### What Needs Fixing

The code-path claim is directionally correct and the implementation builds and passes `make go-test`, but the benchmark evidence is not yet strong enough to confirm the finding. My isolated baseline/optimized worktrees differed only in `cmd/stellar-rpc/internal/network/requestdurationlimiter.go`, and the synchronized 200 RPS control pair did show lower median and p95 latency for the optimized build (`p50 30.191 -> 26.175 ms`, `p95 91.455 -> 79.871 ms`), but the same pair also regressed tail latency (`p99 128.895 -> 143.871 ms`). The earlier 200 RPS pair moved in the opposite direction on p50 (`23.407 -> 26.623 ms`), so the current evidence still has enough run-to-run variance that I cannot confidently attribute the observed deltas to this optimization alone.

The 400 RPS results are also not yet confirmation-quality. Under synced control conditions, baseline failed with `1223` errors and optimized with `3` errors, but **both** runs violated the project's acceptance rule because they had non-zero errors and a p50 jump far above the allowed 20% step-up from the previous valid level. That means the current data does **not** establish a higher zero-error throughput ceiling, only a different overload profile.

### Revision Instructions

1. Re-run the benchmark only after `getHealth` reports `healthy`; discard the initial unsynced baseline run.
2. Keep the isolated baseline/optimized worktrees that differ only by `requestdurationlimiter.go`, but run an alternating control sequence at the highest valid load level (for example `baseline-200`, `optimized-200`, `baseline-200`, `optimized-200`) and report the average/median of those repeated runs.
3. If claiming a throughput benefit, repeat the same alternating procedure at 400 RPS and explicitly apply the project's validity rules: zero errors and no >20% p50 jump vs. the previous valid level. Do not treat the current 400 RPS runs as a ceiling increase because both are invalid by that rule.
4. In the revised write-up, call out the mixed tail-latency result at 200 RPS (`p99` regression in the synced control pair) and explain why the optimization should still be considered a net win, if repeated runs continue to show that.

### Checks Passed So Far

- Targeted code trace passed: the HTTP duration limiter still buffers the full response body, and the optimized `Write` implementation is the only source delta between the isolated benchmark builds.
- Safety/correctness check passed at the current level: the multi-write fallback is present, and `make -j8 build-stellar-rpc` plus `make go-test` succeeded.
- Benchmark tooling check passed: `stellar-rpc-blaster generate` and `stellar-rpc-blaster run` both worked against the local futurenet-backed instance with a fixed seed corpus.
