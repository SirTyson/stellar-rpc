# Success Summary

- 001 `methods/001-batched-ledger-range-reads.md` — **High** — `getTransactions` now batches ledger metadata reads instead of issuing one SQLite lookup per scanned ledger; final review measured an 82.7% latency reduction on a controlled sparse-scan benchmark.
- 002 `network/001-debug-response-marshal-runs-at-info.md` — **Medium** — `getTransactions` no longer marshals successful responses for suppressed debug logging at `info`; final review measured a 6.0% p50 and 11.66% p95 reduction at 100 RPS.
- 003 `network/002-default-info-request-logging.md` — **Low** — `getTransactions` no longer emits per-request JSON-RPC start/finish logs at the default `info` level; final review measured a 4.4% p50 and 6.45% p95 reduction at 100 RPS with no throughput-ceiling gain.
