# H009: `handler.New` reflection in `getTransactions` is not a material network bottleneck

**Date**: 2026-04-07
**Subsystem**: network
**Severity**: Low
**Impact**: request decode CPU
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

If the generic JSON-RPC wrapper is a worthwhile `getTransactions` optimization target, replacing it with an exact-signature handler should remove enough per-request reflection to measurably move endpoint latency or RPS. The wrapper cost has to be large relative to the rest of the request, not just theoretically avoidable.

## Mechanism

I investigated whether `NewGetTransactionsHandler()` paying the generic `handler.New(...)` path is still expensive enough to matter after the method work itself sped up. The wrapper does allocate an argument value, call `req.UnmarshalParams(...)`, and invoke the method through `reflect.Value.Call`, but that work happens only on the tiny request object and does not touch the large response payload that dominates the network-facing cost.

## Trigger

Compare the current `handler.New(transactionsHandler.getTransactionsByLedgerSequence)` wrapper against a handwritten exact-signature `jrpc2.Handler` that unmarshals `protocol.GetTransactionsRequest` directly and calls the method without reflection.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:NewGetTransactionsHandler:330-341` — uses `handler.New(...)` for `getTransactions`
- `cmd/stellar-rpc/internal/methods/handler.go:8-16` — repository helper used by most other RPC methods
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/handler/handler.go:(*FuncInfo).Wrap:163-239` — allocates request args and uses `reflect.Value.Call`
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/handler/handler.go:(*FuncInfo).argWrapper:374-390` — enables the array-wrapper stub for struct params

## Evidence

`getTransactions` is the notable outlier in `cmd/stellar-rpc/internal/methods/`: most other methods use the repository's `NewHandler(...)` helper, while this one still goes through the generic `handler.New(...)` path. The generic wrapper does have unavoidable reflection and object allocation on every call, so it was worth checking.

## Anti-Evidence

The reflection lives entirely on the small parameter-decode side; `json.Unmarshal` of the request body still has to happen either way, and the dominant network-visible costs remain response logging, bridge serialization, and large response buffering. Any saved wrapper work would be at microsecond scale at most against millisecond-to-second `getTransactions` requests.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The generic wrapper does extra reflection, but it only touches the tiny request struct and cannot plausibly deliver a measurable `getTransactions` improvement on its own.

### Lesson Learned

When evaluating wrapper overhead on this endpoint, focus on response-side passes over multi-transaction payloads, not reflection around request decoding.
