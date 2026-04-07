# H001: Reusing the datastore-backed ledger reader for `getTransactions` is not a clear performance win

**Date**: 2026-04-07
**Subsystem**: ingest
**Severity**: Low
**Impact**: remote I/O / historical-scan architecture
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

An optimization for `getTransactions` should reduce latency or CPU for the endpoint’s existing served path. Swapping in a slower historical read plane or adding a new availability path should only count as a viable performance hypothesis if the code strongly suggests it is faster than the current local-DB route for realistic requests.

## Mechanism

I investigated whether `getTransactions` should copy `getLedgers` and read ledger blobs from the datastore-backed `rpcdatastore.LedgerReader` instead of SQLite. The code does show an existing buffered historical reader, but it is optimized for availability and sequential remote reads, not for outperforming local `ledger_close_meta` reads on the current request path; `getTransactions` would still need the same per-ledger transaction parsing after the fetch. That makes this more of a feature-extension idea than a concrete hot-path optimization.

## Trigger

1. Enable `ServeLedgersFromDatastore`.
2. Compare the current `getTransactions` local-DB path against a hypothetical version that fetches its ledgers through `rpcdatastore.LedgerReader`.
3. Focus on whether the remote buffered backend actually beats local SQLite BLOB reads for currently served requests.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_ledgers.go:41-122` — `getLedgers` explicitly supports datastore fallback for ranges outside the local DB.
- `cmd/stellar-rpc/internal/methods/get_ledgers.go:167-260` — fallback path fetches from local DB when available and only uses datastore for uncovered ranges.
- `cmd/stellar-rpc/internal/rpcdatastore/ledger_reader.go:65-88` — datastore reader creates a buffered backend, prepares a range, and fetches ledgers sequentially from remote storage.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:203-310` — `getTransactions` currently assumes local DB reads and has no datastore-aware planner.

## Evidence

There is real adjacent infrastructure here: the daemon wires `DataStoreLedgerReader` into `getLedgers`, and the datastore reader already supports buffered range preparation. That made it plausible that `getTransactions` could share the same historical read plane.

## Anti-Evidence

The current datastore path creates a backend per request and reads fully deserialized `xdr.LedgerCloseMeta` values from remote object storage. For locally retained ledgers, that is not obviously cheaper than SQLite BLOB reads, and for non-local history it primarily extends coverage rather than accelerating the current hot path. I did not find code evidence that the remote path would reduce `getTransactions` latency on its existing served workload.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The datastore reader is an availability-oriented path for historical ledger access, not a demonstrated faster substitute for the local `getTransactions` hot path. The strongest plausible benefit would be broader historical coverage, which is outside this optimization pass.

### Lesson Learned

Adjacent read planes are only viable optimization targets when the code clearly shows they avoid a dominant cost on the existing request path. Reusing the datastore backend here would need proof that remote buffered reads beat local SQLite BLOB reads, not just that the alternate path exists.
