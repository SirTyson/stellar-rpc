# H004: The outer HTTP timeout layer copies the full getTransactions response body

**Date**: 2026-04-06
**Subsystem**: network
**Severity**: High
**Impact**: large-response allocation / copy / GC
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Large `getTransactions` responses should not require the network layer to duplicate the fully serialized JSON body in memory before sending it to the client. Once the method has safely completed within its timeout budget, the response path should avoid a second full-body copy and avoid repeated slice growth while buffering.

## Mechanism

The global HTTP duration limiter wraps the JSON-RPC bridge with `bufferedResponseWriter`, so the entire serialized HTTP body is appended into an in-memory buffer and then copied back out to the real `http.ResponseWriter` in `WriteOut`. This is especially expensive for `getTransactions`, because the endpoint can return up to 200 transactions and each JSON-format transaction includes large envelope/result/meta/event payloads. The result is at least one extra full-body copy plus additional realloc/copy churn as `append` grows the buffer.

## Trigger

Request `getTransactions` with `limit` near the configured maximum and `format=json`, preferably against ledgers with populated events so the serialized body is large. Compare latency, allocations, and RSS/GC behavior before and after bypassing full-body buffering for already-timed inner JSON-RPC responses or replacing it with a pre-sized/streaming-safe strategy.

## Target Code

- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:makeBufferedResponseWriter:75-81` — initializes an empty buffer for all HTTP responses
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:(*bufferedResponseWriter).Write:88-90` — grows the body buffer via `append`
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:(*bufferedResponseWriter).WriteOut:97-118` — writes the fully buffered body back to the real writer
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:(*httpRequestDurationLimiter).ServeHTTP:141-185` — forces the JSON-RPC bridge through the buffering path
- `cmd/stellar-rpc/internal/methods/get_transactions.go:(transactionsRPCHandler).processTransactionsInLedger:152-200` — populates the large per-transaction JSON/XDR fields returned by `getTransactions`
- `cmd/stellar-rpc/internal/methods/get_transactions.go:(transactionsRPCHandler).getTransactionsByLedgerSequence:279-286` — returns the full transaction slice to the bridge for serialization

## Evidence

`getTransactions` can return up to 200 items by default configuration, and the JSON path fills `ResultJSON`, `ResultMetaJSON`, `EnvelopeJSON`, `DiagnosticEventsJSON`, and `Events` for every returned transaction. The outer HTTP limiter buffers that already-large serialized response instead of streaming it, so response size directly amplifies network-layer allocation pressure.

## Anti-Evidence

The buffering is intentional because it prevents partial HTTP output when the outer timeout fires. Small XDR-format responses may not show a dramatic win, so the improvement should be most visible on large JSON `getTransactions` pages rather than every possible request shape.
