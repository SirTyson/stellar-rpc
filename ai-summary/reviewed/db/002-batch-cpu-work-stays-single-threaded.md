# H002: Batch-fetched ledgers are still parsed and encoded on one goroutine

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Medium
**Impact**: CPU latency / multicore underutilization
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

After a `getTransactions` batch has already been materialized into Go memory, CPU-heavy transaction parsing and response encoding should use available cores when the page spans multiple ledgers or many transactions. Large pages should not remain bottlenecked on one goroutine once the SQL read has completed.

## Mechanism

`BatchGetLedgerMetas()` eagerly returns a fully materialized `[]xdr.LedgerCloseMeta`, and the subsequent work in `processTransactionsInLedger()` is pure in-memory CPU: transaction reader setup, per-transaction marshaling, base64 encoding, and optional Rust JSON conversion. The handler currently processes that whole batch sequentially, so dense multi-ledger pages leave cores idle even though the batch prefix needed to satisfy the limit can be computed from `CountTransactions()` and processed independently before the final ordered append.

## Trigger

Request a large page (`limit` near the configured max) over several dense ledgers on a multicore machine, especially in `json` format. A CPU profile should show one request goroutine spending most of its time in `processTransactionsInLedger()`, `db.ParseTransaction()`, `transactionToJSON()`, and event conversion while other cores remain underused.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:getTransactionsByLedgerSequence:314-367` — the outer loop fetches a full batch, then processes each ledger strictly serially
- `cmd/stellar-rpc/internal/db/ledger.go:BatchGetLedgerMetas:118-140` — all ledgers for the batch are already materialized before CPU work begins
- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:76-233` — CPU-only per-ledger parsing and encoding stage

## Evidence

`getTransactionsByLedgerSequence()` fetches `ledgers` first and only then iterates them one by one (`cmd/stellar-rpc/internal/methods/get_transactions.go:330-367`). `BatchGetLedgerMetas()` has already completed the SQL work and returned a Go slice before that loop starts (`cmd/stellar-rpc/internal/db/ledger.go:120-139`). From there on, `processTransactionsInLedger()` only touches in-memory data structures plus local base64/FFI conversion (`cmd/stellar-rpc/internal/methods/get_transactions.go:88-233`), which means the hot stage is parallelizable once the minimal ledger prefix needed for the page is known.

## Anti-Evidence

Small pages that finish in one ledger will not benefit and may regress if goroutine scheduling overhead is added blindly. Any fix has to preserve response ordering, early-stop semantics, and error attribution, so it likely needs to parallelize only the batch prefix that is definitely required.

---

## Review

**Verdict**: VIABLE
**Severity**: Medium
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

The `getTransactionsByLedgerSequence` handler splits into two phases: `fetchLedgerMetas` opens a read-only SQLite transaction and materializes all needed `xdr.LedgerCloseMeta` blobs into a Go slice (tracking cumulative transaction count to stop fetching once enough are collected). Phase 2 iterates this slice sequentially, calling `processTransactionsInLedger` for each ledger. Each call creates a `ledgerTransactionReader` (builds a hash map of all envelopes via SHA-256), then for each transaction performs XDR marshaling (`ParseTransaction`), and for JSON format makes 3 individual CGo/FFI calls (`transactionToJSON` → `ConvertBytes` × 3) plus batched FFI for diagnostic events and contract events. All of this is pure in-memory CPU work with no DB access, yet it runs on a single goroutine.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:275-307` — `getTransactionsByLedgerSequence` Phase 2: sequential `for _, ledger := range collectedMetas` loop, each iteration calls `processTransactionsInLedger`
- `cmd/stellar-rpc/internal/methods/get_transactions.go:312-408` — `fetchLedgerMetas`: accumulates `totalTxCount` per ledger (lines 399-404), proving the runtime already knows which ledgers are "definitely full" before Phase 2 begins
- `cmd/stellar-rpc/internal/methods/get_transactions.go:77-235` — `processTransactionsInLedger`: creates `ledgerTransactionReader` (envelope hashing), iterates transactions, calls `ParseTransaction` + `transactionToJSON` + `jsonifySlice` + `BuildEventsJSONFromTransaction` per transaction (JSON path) or `MarshalBase64` × 3 + event marshaling (XDR path)
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:28-42` — `newLedgerTransactionReader`: builds `envelopesByHash` map by XDR-marshaling and SHA-256 hashing every envelope in the ledger — CPU-only work
- `cmd/stellar-rpc/internal/methods/json.go:12-37` — `transactionToJSON`: 3 separate `xdr2json.ConvertBytes` CGo calls per transaction (result, envelope, meta)
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:36-43,128-147` — `ConvertBytes` → `convertAnyBytes`: each call crosses the CGo boundary, allocates C memory, invokes Rust `xdr_to_json`, copies result back
- `cmd/stellar-rpc/internal/db/transaction.go:238-278` — `ParseTransaction`: XDR `MarshalBinary` for Result, Meta, Envelope + event extraction — pure CPU

### Findings

**The inefficiency is real.** After `fetchLedgerMetas` returns, the Phase 2 loop processes `collectedMetas` strictly sequentially (line 289). Each ledger incurs:

1. **Envelope hash map construction**: `newLedgerTransactionReader` XDR-marshals and SHA-256-hashes every transaction envelope. For a ledger with 20 transactions, this is ~20 marshal+hash operations.

2. **Per-transaction work (JSON format)**: `ParseTransaction` marshals Result/Meta/Envelope to `[]byte`, then `transactionToJSON` makes 3 individual CGo/FFI calls to Rust's XDR-to-JSON converter, `jsonifySlice` batches diagnostic events in one more CGo call, and `BuildEventsJSONFromTransaction` makes 2 more batched CGo calls. Total: ~5-6 CGo boundary crossings per transaction.

3. **Per-transaction work (XDR format)**: `MarshalBase64` × 3 + diagnostic event marshaling + transaction event marshaling. Cheaper than JSON but still non-trivial at scale.

**The parallelization boundary is clean.** Each `processTransactionsInLedger` call operates on an independent `xdr.LedgerCloseMeta` with no shared mutable state — the only shared objects are `txns` (append-only slice) and `enc` (XDR encoding buffer, not thread-safe). Per-goroutine copies of `enc` and per-goroutine result slices with post-loop merge preserve correctness.

**The "definitely full" prefix is already computable.** `fetchLedgerMetas` accumulates `totalTxCount` per ledger (lines 399-404). By comparing cumulative counts against `limit`, the handler can identify ledgers 0..N-1 as definitely fully consumed (all their transactions appear in the result) and ledger N as potentially partial. Ledgers 0..N-1 can be processed in parallel; ledger N is processed serially with the remaining limit.

**Thread safety of the FFI layer is acceptable.** The Rust `xdr_to_json` function takes an XDR type name and byte buffer as input, parses XDR, and returns JSON — it is a pure function with no global mutable state. Multiple goroutines can safely make concurrent CGo calls to this function. Each CGo call runs on its own OS thread (Go runtime behavior), enabling true multicore utilization.

**Impact is concentrated on JSON format with large pages.** For a page of 200 transactions across 10 dense ledgers in JSON format, the sequential CPU phase dominates request latency. With ~5-6 CGo crossings per transaction at ~50-100μs each, the per-ledger cost is roughly 5-10ms. Parallelizing 10 ledgers across 4 cores reduces the CPU phase by ~60-75%. For XDR format, the improvement is smaller but still measurable for large pages.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/methods/get_transactions.go` — the Phase 2 loop at lines 289-297 in `getTransactionsByLedgerSequence`
- **Change description**: Before the Phase 2 loop, compute the "definitely full" ledger prefix by iterating `collectedMetas` and summing `CountTransactions()` until reaching `limit`. Process ledgers 0..N-1 in parallel (each goroutine gets its own `xdr.EncodingBuffer` and appends to a local `[]protocol.TransactionInfo`), then merge results in ledger order. Process the potentially partial last ledger serially with the remaining limit. Cap concurrency at `runtime.GOMAXPROCS(0)` or a fixed limit (e.g., 8) to avoid excessive goroutine overhead for small batches. Skip parallelization entirely when `len(collectedMetas) <= 1`.
- **Correctness check**: Existing `getTransactions` tests should continue to pass. The result slice must be in the same ledger-then-transaction order. Cursor, error, and early-stop behavior must be identical. Verify with `go test ./cmd/stellar-rpc/internal/methods/... -run TestGetTransactions`
- **Benchmark focus**: Measure `getTransactions` latency for large pages (limit=200) in JSON format over dense ledger ranges on a multicore machine. Target metric: p50/p99 latency reduction of 5-20%. Also measure XDR format for comparison (expect smaller improvement). Compare single-ledger vs. multi-ledger pages to confirm no regression on the single-ledger path.
