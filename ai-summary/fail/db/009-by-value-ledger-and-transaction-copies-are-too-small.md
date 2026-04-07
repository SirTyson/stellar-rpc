# H009: By-value ledger and transaction copies are too small to matter

**Date**: 2026-04-07
**Subsystem**: db
**Severity**: Low
**Impact**: copy overhead
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

If `getTransactions` is paying meaningful copy overhead in the hot path, it should avoid passing very large structs by value between the reader, parser, and response builders. Pointer-heavy ledger/transaction state should not be cloned repeatedly per returned transaction unless that copying is a measurable fraction of request time.

## Mechanism

I suspected that `processTransactionsInLedger()`, `ledgerTransactionReader.Read()`, and `db.ParseTransaction()` might be copying large `xdr.LedgerCloseMeta` / `ingest.LedgerTransaction` values on every transaction. If those structs were large enough, converting the boundary to pointers could have reduced per-transaction memory traffic and stack copying.

## Trigger

Call `getTransactions` in `json` format on dense ledgers and inspect whether value copies of ledger and transaction structs show up as a hot cost in profiles or size measurements.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:77-235` — passes `ledger xdr.LedgerCloseMeta` by value into the per-ledger helper
- `cmd/stellar-rpc/internal/methods/ledger_transaction_reader.go:Read:44-74` — returns `ingest.LedgerTransaction` by value
- `cmd/stellar-rpc/internal/db/transaction.go:ParseTransaction:238-277` — accepts both `xdr.LedgerCloseMeta` and `ingest.LedgerTransaction` by value

## Evidence

The signatures above all use by-value parameters or returns, so there is real copy traffic on paper. I measured the actual sizes with a temporary Go snippet in this repo's module context and found `unsafe.Sizeof(xdr.LedgerCloseMeta{}) == 32` and `unsafe.Sizeof(ingest.LedgerTransaction{}) == 280`, which means the copied payloads are mostly small headers and pointers rather than the full deep XDR object graphs.

## Anti-Evidence

The surrounding work on the same path is much larger: full `LedgerCloseMeta` decode, eager envelope hashing, `MarshalBinary()` of result/meta/envelope/events, base64 encoding, and optional Rust JSON conversion. Even eliminating these value copies entirely would only shave a tiny constant from each transaction and would not compete with the dominant serialization and hashing costs.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The measured structs are far smaller than the underlying ledger data they reference, so the hot path is not spending a meaningful share of time on by-value copying.

### Lesson Learned

For this endpoint, distinguish header copies from deep XDR copies. Small union/slice-header structs can look suspicious in signatures, but the real wins come from eliminating decoding, hashing, or serialization passes over the underlying payloads.
