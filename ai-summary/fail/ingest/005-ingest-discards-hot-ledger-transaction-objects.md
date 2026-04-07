# H003: Ingest discards fully materialized `LedgerTransaction` objects that the next `getTransactions` request rebuilds

**Date**: 2026-04-07
**Subsystem**: ingest
**Severity**: Medium
**Impact**: CPU / hashing / allocation / hot-ledger first-hit latency
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

The first `getTransactions` request for a freshly closed ledger should be able to reuse the transaction objects ingest already materialized while indexing that ledger. Hot recent requests should not need to rebuild a new ledger transaction reader, rehash every envelope, and reconstruct `ingest.LedgerTransaction` values when ingest produced those same objects moments earlier.

## Mechanism

`transactionHandler.InsertTransactions()` already walks the ledger in application order with `reader.Read()` and obtains full `ingest.LedgerTransaction` values; it then keeps only hash/index data for the DB insert and discards the richer objects. `getTransactions` later calls `newLedgerTransactionReader()`, which eagerly hashes all envelopes into `envelopesByHash` and reconstructs a second stream of `ingest.LedgerTransaction` values before `ParseTransaction()` can run. A bounded ingest-fed recent cache of ordered `[]ingest.LedgerTransaction` keyed by ledger sequence would let the first post-close request skip that entire reconstruction phase and go straight to pagination, `ParseTransaction()`, and output formatting.

## Trigger

1. Run tip-following ingest with dense ledgers.
2. Issue `getTransactions` requests against the newest ledger immediately after close, especially with small limits or multiple concurrent pollers.
3. Compare current latency against a version that checks a recent ingest-populated `[]ingest.LedgerTransaction` cache before calling `newLedgerTransactionReader()`.

## Target Code

- `cmd/stellar-rpc/internal/db/transaction.go:88-110` — `InsertTransactions()` already reads full `ingest.LedgerTransaction` values in order.
- `cmd/stellar-rpc/internal/ingest/service.go:208-224` — ingest commits the ledger, then retains only sequence metadata.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:89-130` — request path rebuilds the transaction stream for every touched ledger.
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:28-104` — `newLedgerTransactionReader()` eagerly rebuilds the envelope-hash map and re-materializes transactions.
- `cmd/stellar-rpc/internal/db/transaction.go:238-277` — `ParseTransaction()` already accepts `ingest.LedgerTransaction`, so a cached slice fits the existing formatter path.

## Evidence

The write path already pays the SDK reader walk and has the exact transaction objects the read path wants. Nothing in `getTransactions` requires those objects to have been rebuilt locally; it only needs application-order `ingest.LedgerTransaction` values to feed into `ParseTransaction()` or direct XDR encoding. Retaining a small recent slice is therefore a true reuse of already-computed work, not a proposal to move new formatting work onto ingest.

## Anti-Evidence

These objects are large and only benefit recent ledgers that are queried while still hot in the cache. JSON formatting, event encoding, and base64 conversion still dominate for some workloads, so the end-to-end gain depends on how much reader reconstruction contributes for dense ledgers.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: FAIL — duplicate of reviewed/ingest/001-hot-envelope-order-cache-skips-rehash.md and fail/ingest/003-ingest-retained-transaction-artifacts-skip-hot-reparse.md
**Failed At**: reviewer

### Trace Summary

Traced the ingest write path (`InsertTransactions` at transaction.go:88-110 using SDK's `ingest.NewLedgerTransactionReaderFromLedgerCloseMeta`) and the read path (`processTransactionsInLedger` at get_transactions.go:88-129 using the custom `newLedgerTransactionReader` at ledger_transaction_reader.go:28-104). Confirmed that caching `ingest.LedgerTransaction` objects would save the envelope hashing phase (`storeTransactions` at ledger_transaction_reader.go:86-104) plus the trivial per-transaction `Read()` construction (ledger_transaction_reader.go:44-74). However, this optimization is already covered by two prior investigations.

### Code Paths Examined

- `cmd/stellar-rpc/internal/db/transaction.go:88-110` — `InsertTransactions` uses SDK reader, walks transactions, extracts only hash+index for DB insert. Full `ingest.LedgerTransaction` objects are produced then discarded — this part of the hypothesis is factually correct.
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:28-104` — Custom reader eagerly hashes all envelopes via `storeTransactions` → `hashTransactionInEnvelopeWithID`. Per-envelope cost: XDR marshal of `TransactionSignaturePayload` (~200-500 bytes) + SHA-256 hash. For 200 transactions, ~1-2ms total.
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:44-74` — `Read()` does a map lookup + field assignment from LCM accessors. Negligible cost per call.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:140-200` — Per-transaction processing: `ParseTransaction` (3× `MarshalBinary` on Result/Meta/Envelope + event extraction) or `MarshalBase64` calls. This is the dominant per-request cost and is NOT addressed by caching `ingest.LedgerTransaction` objects.

### Why It Failed

This hypothesis is a duplicate that falls between two existing investigations:

1. **Envelope hashing avoidance is already covered by reviewed/ingest/001-hot-envelope-order-cache-skips-rehash.md** (VIABLE/Low). That hypothesis specifically targets the `storeTransactions` envelope hashing phase and proposes caching `map[xdr.Hash]xdr.TransactionEnvelope` for recent ledgers. It was reviewed and accepted as VIABLE/Low with PoC guidance. The current hypothesis's core optimization (skip `newLedgerTransactionReader()` construction) is the same optimization.

2. **The broader "cache full transaction objects from ingest" claim was already analyzed in fail/ingest/003-ingest-retained-transaction-artifacts-skip-hot-reparse.md** (NOT_VIABLE). That review traced the same code paths and established that: (a) the only shared cost between ingest and read paths is `LedgerTransactionReader` construction, (b) the dominant read-path cost (`ParseTransaction` marshaling) is exclusively a read-path operation not performed during ingest, and (c) the incremental benefit of caching `ingest.LedgerTransaction` objects beyond an envelope-map cache is negligible since `Read()` is just a map lookup + field assignment.

The current hypothesis proposes caching `[]ingest.LedgerTransaction` — this is functionally equivalent to caching the envelope map (reviewed H001) plus the trivial `Read()` output. The envelope map caching is already accepted; the additional `Read()` skip adds negligible value.

### Lesson Learned

When a hypothesis proposes caching a composite object (e.g., `[]ingest.LedgerTransaction`), decompose it into its constituent costs. The expensive component (envelope hashing) may already be addressed by a prior investigation, leaving only trivial remaining savings (field assignment from LCM accessors).
