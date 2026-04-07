# H005: getTransactions Keeps Burning CPU After the Request Context Has Already Been Canceled

**Date**: 2026-04-06
**Subsystem**: methods
**Severity**: Medium
**Impact**: RPS / wasted CPU under overload
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once the request deadline expires or the client disconnects, `getTransactions` should stop before more transaction parsing, XDR marshaling, base64 work, or JSON conversion. Cancellation should cut off the hot path quickly enough that overloaded nodes do not spend queue capacity on responses that can no longer be delivered.

## Mechanism

`getTransactionsByLedgerSequence` passes `ctx` into DB reads, but `processTransactionsInLedger` does not accept a context and never checks `ctx.Err()` while iterating transactions and doing expensive serialization work. The server enforces a per-method execution deadline in `jsonrpc.go`, so once a request is canceled the handler can still continue decoding the current ledger and running JSON/FFI conversion for transactions that will be discarded. Under timeout-heavy or disconnect-heavy load, that wasted CPU should reduce effective RPS.

## Trigger

1. Configure a short `max-get-transactions-execution-duration` or cancel client requests mid-flight.
2. Send large `getTransactions` requests, especially `format=json`, against ledgers with enough transactions/events to keep the CPU busy.
3. Measure CPU time spent after cancellation and compare against a version that checks `ctx.Err()` at the start of each ledger and transaction iteration.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:94-214` — transaction loop has no context parameter and no cancellation checks.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:218-276` — outer ledger loop only observes cancellation at DB fetch boundaries.
- `cmd/stellar-rpc/internal/jsonrpc.go:246-251` — `getTransactions` is explicitly wrapped with a request-duration limit.

## Evidence

The endpoint has an execution timeout configured at the JSON-RPC layer, which means cancellation is a normal, expected control-flow path rather than a rare edge case. The hottest CPU work in the handler happens after each DB fetch, exactly where the code stops consulting the context.

## Anti-Evidence

If requests rarely time out or disconnect, this optimization may not change median latency. DB operations already receive the context, so the waste is concentrated in post-fetch CPU work rather than in blocked I/O.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-06
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

Traced the full cancellation path from the JSON-RPC duration limiter through the handler goroutine. The `RPCRequestDurationLimiter.Handle` (requestdurationlimiter.go:233) creates a `context.WithTimeout` (line 251) and runs the handler in a goroutine (line 254). When the timeout fires (line 272), it calls `requestCtxCancel()` and returns an error to the client, but the handler goroutine continues running. Inside `getTransactionsByLedgerSequence`, `fetchLedgerData` passes the canceled context to the DB and would fail fast on the next call, but `processTransactionsInLedger` receives no context and processes all remaining transactions in the current ledger before the outer loop can observe cancellation.

### Code Paths Examined

- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:233-303` — `Handle` creates `context.WithTimeout`, runs handler in goroutine, returns error on timeout but goroutine continues
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:251` — `requestCtx, requestCtxCancel := context.WithTimeout(ctx, q.limitThreshold)` — the canceled context
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:262` — handler goroutine passes `requestCtx` to the handler function
- `cmd/stellar-rpc/internal/methods/get_transactions.go:258-277` — outer ledger loop; `fetchLedgerData(ctx, ...)` at line 265 will catch canceled context on next iteration
- `cmd/stellar-rpc/internal/methods/get_transactions.go:94-214` — `processTransactionsInLedger` has no `ctx` parameter; no `ctx.Err()` check in the per-transaction loop (lines 130-211)
- `cmd/stellar-rpc/internal/methods/get_transactions.go:162-200` — format-dependent serialization (JSON path calls `transactionToJSON`, `jsonifySlice`, `BuildEventsJSONFromTransaction`); this is the CPU-intensive work wasted after cancellation
- `cmd/stellar-rpc/internal/methods/json.go:12-37` — `transactionToJSON` makes 3 `xdr2json.ConvertBytes` calls per transaction (result, envelope, meta)
- `cmd/stellar-rpc/internal/config/options.go:576-579` — `MaxGetTransactionsExecutionDuration` defaults to 5 seconds, confirming timeouts are a designed control-flow path

### Findings

**The inefficiency exists and is real.** After the duration limiter cancels the context, the handler goroutine continues executing `processTransactionsInLedger` for all remaining transactions in the current ledger. The waste is bounded by ONE ledger's worth of transaction processing because `fetchLedgerData(ctx, ...)` on the next loop iteration will return `context.Canceled` immediately.

**Impact is bounded but real under overload.** With `format=json`, each transaction requires 3 XDR-to-JSON conversions (`transactionToJSON`), diagnostic event conversion (`jsonifySlice`), and event building (`BuildEventsJSONFromTransaction`). For a dense ledger with ~100 transactions, this could waste 10-50ms of CPU per timed-out request. Under sustained overload with many concurrent timeouts, the compound CPU waste reduces available capacity for new requests.

**Why Low rather than Medium:** The waste is bounded by one ledger per canceled request. The `fetchLedgerData` ctx check prevents multi-ledger waste. Under normal operation (no timeouts), there is zero benefit. The RPS improvement from this fix would only manifest under sustained overload conditions where many requests simultaneously time out — a scenario where other bottlenecks (queue limits, connection limits) likely dominate. The default transaction limit is 50 and the default execution duration is 5s, so most requests complete well within the timeout.

**The fix is trivially correct.** Adding `ctx context.Context` to `processTransactionsInLedger` and checking `ctx.Err()` at the top of the per-transaction loop (line 130) introduces no correctness risk — the result is already being discarded by the duration limiter. An additional `ctx.Err()` check at the top of the outer ledger loop (before `fetchLedgerData`) would provide even faster cancellation at negligible cost.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/methods/get_transactions.go` — modify `processTransactionsInLedger` to accept `ctx context.Context` and add `if err := ctx.Err(); err != nil { return cursor, false, err }` at the top of the loop at line 130. Also add a `ctx.Err()` check at the top of the outer ledger loop in `getTransactionsByLedgerSequence` (before line 265).
- **Change description**: Pass the request context into `processTransactionsInLedger` and check cancellation at the start of each transaction iteration. This allows the handler goroutine to exit promptly after the duration limiter cancels the context, instead of processing an entire ledger's worth of transactions that will be discarded.
- **Correctness check**: Existing tests in `get_transactions_test.go` should continue to pass since cancellation checks only trigger on canceled contexts, which don't occur in normal test flows. Add a specific test with a pre-canceled context to verify early exit.
- **Benchmark focus**: Measure CPU time consumed by timed-out `getTransactions` requests (especially `format=json`) against dense ledgers. The metric to watch is total CPU-seconds wasted per timeout event. Compare by issuing many concurrent requests with a short `max-get-transactions-execution-duration` (e.g., 50ms) against ledgers with 50+ transactions. Expect the patched version to release goroutines faster and show lower CPU utilization under overload.

---

## PoC Attempt

**Result**: POC_PASS
**Date**: 2026-04-07
**PoC by**: claude-opus-4.6, high

### Changes Made

- `cmd/stellar-rpc/internal/methods/get_transactions.go:75-76` — Added `ctx context.Context` as the first parameter to `processTransactionsInLedger`.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:112-115` — Added `if err := ctx.Err(); err != nil { return cursor, false, err }` at the top of the per-transaction loop, before any expensive serialization work (XDR parsing, JSON conversion, base64 encoding).
- `cmd/stellar-rpc/internal/methods/get_transactions.go:247-250` — Added `if err := ctx.Err(); err != nil { return ..., err }` at the top of the outer batch loop in `getTransactionsByLedgerSequence`, providing early exit before even fetching ledger data from the DB.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:290` — Updated the call site to pass `ctx` through to `processTransactionsInLedger`.

### Demonstration

The optimization threads the request context into the per-transaction processing loop and checks for cancellation at two points: before each batch of ledger fetches and before each transaction's expensive serialization work. When the JSON-RPC duration limiter cancels the context (via `context.WithTimeout`), the handler goroutine now exits promptly instead of processing all remaining transactions in the current ledger. This eliminates wasted CPU on XDR-to-JSON conversions, base64 encoding, and event building for responses that will be discarded.

### Test Results

All unit tests in `cmd/stellar-rpc/internal/methods/` pass (including all 8 `TestGetTransactions_*` tests covering default limits, custom limits, cursor pagination, JSON format, error cases, and empty results). Tests run with `-race` enabled and complete in ~1.5s.

---

## Final Review

**Verdict**: REJECTED
**Date**: 2026-04-07
**Final review by**: gpt-5.4, high
**Failed At**: final-review

### Adversarial Analysis

1. **Exercises claimed inefficiency**: YES. `requestdurationlimiter.go:251-282` cancels the request context on timeout and returns `-32001`, while `get_transactions.go:112-115` and `247-250` now only add `ctx.Err()` checks after that cancellation path exists.
2. **Realistic preconditions**: LIMITED. The optimization only matters once `getTransactions` requests are already timing out or disconnecting; it does not affect normal successful requests.
3. **Inefficiency vs by-design**: INEFFICIENCY. The pre-patch code could keep serializing transactions after cancellation.
4. **Final severity**: NOT SUPPORTED. Using `stellar-rpc-blaster` with the project flow and an isolated baseline that preserved the unrelated JSON/FFI worktree changes, both variants had the same zero-error throughput ceiling: 200 RPS. At 300 RPS the baseline produced 98 timeout errors, while the optimized variant produced 145; at 400 RPS the baseline produced 131 errors, while the optimized variant produced 287.
5. **In scope**: YES. The change is in the `getTransactions` handler.
6. **Benchmark methodology**: CORRECT. I built both variants, used the project's blaster, reused the same seed data, and isolated the review to the cancel-check delta because the live worktree also contained unrelated `json.go` / `xdr2json` performance edits.
7. **Alternative explanations**: PRESENT. The only zero-error latency improvement was at 200 RPS (`p50` 29.999ms -> 27.919ms), but the same change was slower at 100 RPS (`p50` 14.727ms -> 15.455ms). Because the added `ctx.Err()` checks only run differently after cancellation, any apparent improvement during zero-error successful runs is measurement noise rather than evidence for this optimization.
8. **Novelty**: PASS.

### Rejection Reason

The fix addresses a real inefficiency, but the performance claim is not supported by the required benchmark. The maximum zero-error throughput stayed flat at 200 RPS, and once timeout-driven cancellation started at 300+ RPS the optimized variant was not better and was often worse.

### Failed Checks

- 4
- 7
