# H014: Envelope Cache Lock Contention Is Too Small to Matter

**Date**: 2026-04-07
**Subsystem**: methods
**Severity**: Low
**Impact**: RPS / lock contention
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

The shared envelope cache in `getTransactions` should reduce repeated envelope hashing without turning concurrent requests into a lock convoy. If the cache lock were materially hot, replacing it with an `RWMutex` or sharded cache should improve concurrent request throughput.

## Mechanism

At first glance, `envelopeCache.get()` and `put()` look suspicious because they use a single `sync.Mutex` for every cache hit and miss. If those critical sections wrapped the expensive work of hashing transaction envelopes or building the per-ledger map, concurrent polling of the same tip ledgers could serialize on that lock and erase the cache benefit.

## Trigger

1. Send many concurrent `getTransactions` requests against the same recently cached ledgers.
2. Compare throughput and mutex profiles against a version that swaps the cache to `sync.RWMutex`.

## Target Code

- `cmd/stellar-rpc/internal/methods/envelope_cache.go:get:41-46` — cache hit path uses an exclusive mutex.
- `cmd/stellar-rpc/internal/methods/envelope_cache.go:put:48-69` — cache miss insertion also uses the same mutex.
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:37-59` — `newLedgerTransactionReader()` calls `get()`/`put()` around reader creation.

## Evidence

Every selected ledger touches the cache, and the lock is process-wide for the handler instance. That made lock contention a plausible concurrency regression when I first read the cache code.

## Anti-Evidence

The lock scope is tiny: `get()` is only a single map lookup, and `put()` is only map/ring bookkeeping. The expensive work — `TransactionEnvelopes()`, per-envelope hashing, and map construction in `storeTransactions()` — happens entirely outside the critical section, and cache hits return the existing map by reference without copying it.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The mutex is real, but the protected work is far too small to compete with the surrounding hot-path costs. Changing the cache to `RWMutex` might shave a few nanoseconds per touched ledger; it will not produce a measurable end-to-end `getTransactions` improvement next to ledger blob fetches, XDR unmarshal, envelope hashing on misses, and transaction serialization.

### Lesson Learned

For shared caches, inspect the exact lock scope before treating the lock type itself as the bottleneck. A global mutex only matters here if it encloses the hashing or decode work; in this implementation it guards only pointer-sized map metadata operations.
