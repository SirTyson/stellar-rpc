# H001: Successful `getTransactions` responses are JSON-marshaled again for debug logging even at default `info` level

**Date**: 2026-04-06
**Subsystem**: network
**Severity**: High
**Impact**: serialization CPU / allocation
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When debug logging is disabled, the `getTransactions` request path should not serialize the successful response body for logging. The endpoint should pay the mandatory JSON serialization cost exactly once, when the JSON-RPC bridge encodes the response that will actually be sent to the client.

## Mechanism

`decorateHandlers` calls `logResponse()` before returning the handler result to the `jhttp` bridge. For every successful request, `logResponse()` unconditionally executes `json.Marshal(response)` and then `string(responseBytes)` before issuing a `logger.Debug(...)` call, even though the default `--log-level` is `info`. On `getTransactions`, this means large result pages can be fully marshaled once for discarded debug data and then marshaled again by the bridge for the real HTTP response, with an additional full byte-to-string copy in between.

## Trigger

Run `getTransactions` with `format=json` and `limit` near the configured maximum while leaving the server at the default `--log-level info`. Compare CPU time, heap allocation, and latency before and after gating the response-marshaling block on an actual debug-level check.

## Target Code

- `cmd/stellar-rpc/internal/jsonrpc.go:decorateHandlers:83-103` — calls `logResponse()` before handing the result back to the bridge
- `cmd/stellar-rpc/internal/jsonrpc.go:logResponse:124-141` — always marshals successful responses and copies them into a string for a debug-only field
- `cmd/stellar-rpc/internal/config/options.go:log-level option:140-143` — default log level is `info`, so the debug message is normally suppressed
- `cmd/stellar-rpc/internal/methods/get_transactions.go:(transactionsRPCHandler).processTransactionsInLedger:152-200` — populates the large per-transaction JSON payloads that make the duplicate marshal expensive
- `cmd/stellar-rpc/internal/config/options.go:max-transactions-limit option:360-364` — allows up to 200 transactions in one `getTransactions` page

## Evidence

The wrapper logs every successful response through `logResponse()`, and the response-marshaling work happens before any debug-level filtering. `getTransactions` can return large JSON-heavy payloads (`ResultJSON`, `ResultMetaJSON`, `EnvelopeJSON`, diagnostic events, and event bodies) for as many as 200 transactions, so the duplicate marshal is on the hot path for the largest responses that matter most for latency.

## Anti-Evidence

Error responses do not take this path, and small XDR-format pages will see a smaller absolute win. If an operator explicitly runs at debug level, the logging output is intentional, although the code would still benefit from avoiding the extra byte-to-string copy when possible.

---

## Review

**Verdict**: VIABLE
**Severity**: Medium
**Date**: 2026-04-06
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

The `decorateHandlers` closure in `jsonrpc.go:83-104` wraps every JSON-RPC method handler. On every successful call it invokes `logResponse()` at line 102 before returning the result. Inside `logResponse()` (lines 124-142), when `status == "ok"`, the function unconditionally calls `json.Marshal(response)` and then `string(responseBytes)` — both execute eagerly before `logger.Debug()` checks the log level. Since the default log level is `info` (configured in `config/options.go:143`), the debug message is always discarded, making both the marshal and the byte-to-string copy pure waste. The jhttp bridge then performs its own `json.Marshal` on the same result for the actual HTTP response, so every successful request pays the full serialization cost twice.

### Code Paths Examined

- `cmd/stellar-rpc/internal/jsonrpc.go:decorateHandlers:82-108` — wraps all handlers; calls `logResponse(logger, reqID, duration, label["status"], result)` at line 102 before returning result at line 103
- `cmd/stellar-rpc/internal/jsonrpc.go:logResponse:124-142` — at line 135 unconditionally calls `json.Marshal(response)`, at line 138 copies bytes to string via `string(responseBytes)`, only then calls `logger.Debug()` at line 139 where the log-level check discards the message
- `go-stellar-sdk@v0.4.0/support/log/entry.go:Debug:131-133` — delegates to `logrus.Entry.Debug()` which checks the level internally, but by this point the marshal has already executed
- `go-stellar-sdk@v0.4.0/support/log/entry.go:WithField:71-75` — eagerly creates a new entry with the field value; no lazy evaluation
- `go-stellar-sdk@v0.4.0/protocols/rpc/get_transactions.go:42-90` — `TransactionDetails` contains `json.RawMessage` fields (EnvelopeJSON, ResultJSON, ResultMetaJSON, DiagnosticEventsJSON) which are already serialized JSON; `GetTransactionsResponse` holds up to 200 `TransactionInfo` structs
- `cmd/stellar-rpc/internal/config/options.go:140-164` — default log level is `logrus.InfoLevel`

### Findings

1. **The inefficiency is confirmed**: `json.Marshal(response)` and `string(responseBytes)` execute unconditionally on every successful request, regardless of log level. The logrus level check occurs inside `Debug()` after the marshal.

2. **It is on the hot path**: Every successful `getTransactions` call traverses this code. The marshal runs once per request in `logResponse`, then the jhttp bridge performs an identical marshal for the HTTP response — a full duplication.

3. **Response size is significant**: `GetTransactionsResponse` can contain up to 200 `TransactionInfo` entries. Each entry with `format=json` contains `json.RawMessage` fields for envelope, result, result meta, and diagnostic events. While `json.RawMessage` fields are cheap to marshal (byte copy), the aggregate response can be several MB. The `string()` conversion at line 138 creates an additional full copy.

4. **Allocation waste**: Each request produces two transient allocations (marshal output `[]byte` + string copy) that become garbage immediately since the debug log is suppressed. At high RPS with large responses, this creates meaningful GC pressure.

5. **No existing optimization covers this**: There is no level check, caching, or lazy evaluation gating the marshal. The `log.Entry` wrapper does not expose `IsLevelEnabled()` or `GetLevel()`, so the fix requires either a minor approach change or a small upstream addition.

6. **Severity adjustment**: Downgraded from High to Medium. The wasted marshal is real and per-request, but `json.RawMessage` fields are essentially byte copies (not full re-encoding), and the total marshal cost is a fraction of overall request time which includes DB reads, XDR parsing, and transaction processing. The improvement is meaningful (eliminates one full response serialization + copy) but likely 5–20% of total request latency rather than >20%.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/jsonrpc.go`, function `logResponse` lines 134-141
- **Change description**: Gate the `json.Marshal` + `string()` block on a debug-level check. Since `log.Entry` does not expose `IsLevelEnabled()`, two approaches:
  1. **Simplest (no upstream change)**: Remove the `json.Marshal` block entirely from `logResponse`, or pass a `debugEnabled bool` parameter computed once at handler setup time based on the configured log level.
  2. **Cleaner (minor upstream change)**: Add `func (e *Entry) IsLevelEnabled(level logrus.Level) bool` to `go-stellar-sdk/support/log/entry.go`, then guard the block with `if logger.IsLevelEnabled(logrus.DebugLevel)`.
- **Correctness check**: Existing tests for `getTransactions` should continue to pass since this only affects debug logging output. No behavioral change for the response itself.
- **Benchmark focus**: Measure allocations per `getTransactions` request (bytes allocated, allocs/op) and p50/p99 latency with `format=json` and `limit=200` at the default info log level. Expect ~50% reduction in marshal-related allocations and a measurable latency improvement proportional to response size.

---

## PoC Attempt

**Result**: POC_PASS
**Date**: 2026-04-07
**PoC by**: claude-opus-4-6, high

### Changes Made

- **`cmd/stellar-rpc/internal/jsonrpc.go:74`** — Added `debugEnabled bool` parameter to `decorateHandlers` function signature. This flag is captured by the handler closure and passed through to `logResponse`.

- **`cmd/stellar-rpc/internal/jsonrpc.go:103`** — Updated `logResponse` call to pass the `debugEnabled` flag.

- **`cmd/stellar-rpc/internal/jsonrpc.go:125-143`** — Modified `logResponse` to accept `debugEnabled bool` parameter. The `json.Marshal` + `string()` block is now gated by `debugEnabled &&` before the `status == "ok"` check, so at the default `info` log level, neither the marshal nor the byte-to-string copy execute.

- **`cmd/stellar-rpc/internal/jsonrpc.go:20`** — Added `"github.com/sirupsen/logrus"` import for the `logrus.DebugLevel` constant.

- **`cmd/stellar-rpc/internal/jsonrpc.go:326-329`** — Updated the `decorateHandlers` call in `NewJSONRPCHandler` to pass `cfg.LogLevel >= logrus.DebugLevel`, computing the debug-enabled flag once at handler setup time from the config.

### Demonstration

The optimization eliminates an unconditional `json.Marshal(response)` + `string(responseBytes)` that executed on every successful JSON-RPC request, even when the resulting debug log message was always discarded at the default `info` log level. By computing a `debugEnabled` flag once at handler construction time from `cfg.LogLevel`, the entire marshal + byte-to-string copy block is skipped for all requests when running at `info` level or above, removing one full duplicate serialization pass and its associated allocations from the hot path.

### Test Results

All 16 Go test packages in `cmd/stellar-rpc/internal/...` pass (with `-race`), including config, db, feewindow, ingest, integrationtest, ledgerbucketwindow, methods, network, preflight, rpcdatastore, util, and xdr2json.
