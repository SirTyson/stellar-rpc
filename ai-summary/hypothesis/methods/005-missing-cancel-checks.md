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
