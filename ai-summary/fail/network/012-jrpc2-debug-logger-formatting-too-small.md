# H012: jrpc2 bridge debug logger formatting is too small to matter for `getTransactions`

**Date**: 2026-04-07
**Subsystem**: network
**Severity**: Low
**Impact**: suppressed debug-format CPU
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

If the bridge's debug logging callback is worth optimizing for `getTransactions`, turning it off at non-debug levels should remove enough hot-path work to measurably change endpoint latency or throughput. A viable finding needs that formatting cost to be material relative to the rest of the network path.

## Mechanism

I investigated whether wiring `BridgeOptions.Server.Logger` to `params.Logger.Debug` causes meaningful wasted work at the default `info` log level. jrpc2's logger adapter does eagerly `fmt.Sprintf(...)` before invoking the callback, so there is real suppressed-debug formatting on the request path; the question is whether those few short formatted strings per request are large enough to matter for `getTransactions`.

## Trigger

Profile `getTransactions` at `--log-level info` and isolate time spent in jrpc2 `Logger.Printf` formatting after disabling only the bridge server logger callback.

## Target Code

- `cmd/stellar-rpc/internal/jsonrpc.go:158-162` — stellar-rpc unconditionally provides a jrpc2 bridge server logger callback
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/opts.go:52-57` — a non-nil server logger becomes `Logger.Printf`
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/opts.go:213-216` — `Logger.Printf` eagerly performs `fmt.Sprintf`
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/server.go:212-213` — normal request path logs dequeues
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/server.go:288-303` — normal request path logs completion
- `/home/garand/go/pkg/mod/github.com/creachadair/jrpc2@v1.3.3/server.go:661-661` — normal request path logs receipt of request batches

## Evidence

The eager formatting is real: unlike a nil logger, a provided jrpc2 logger always formats its string before our `Debug(...)` callback gets a chance to discard it. The bridge's local server emits a few such messages on the normal request path.

## Anti-Evidence

Only the server-side jrpc2 logger is wired in stellar-rpc; the local client logger is nil. The normal path therefore pays only a handful of tiny `fmt.Sprintf` calls per request and no extra I/O, which is far too small next to the already-confirmed costs in response logging, bridge round-tripping, and response buffering.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The wasted debug formatting exists, but it is only a few short strings on the server side of the local bridge and does not plausibly clear the measurable-impact bar for `getTransactions`.

### Lesson Learned

For bridge logging work in this subsystem, the only worthwhile targets are request/response logs or other paths that touch the full payload; tiny suppressed debug strings are noise.
