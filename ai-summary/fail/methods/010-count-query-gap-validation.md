# H010: A Count Query Could Replace the Per-Batch Missing-Ledger Map

**Date**: 2026-04-07
**Subsystem**: methods
**Severity**: Low
**Impact**: CPU / allocations
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Gap validation in `getTransactions` should detect missing ledgers with minimal per-batch overhead. The handler ideally would not allocate a temporary map and walk the whole batch when a simpler metadata check can prove the batch is contiguous.

## Mechanism

The batch loop currently builds `ledgerMap := make(map[uint32]bool, len(ledgers))` and then scans every expected sequence to locate the first missing ledger. Since `ledger.go` already has `getLedgerCountInRange`, it looked plausible that a cheap `COUNT/MIN/MAX` query might replace the map-building step and reduce hot-path allocations.

## Trigger

1. Run `getTransactions` over a long sparse range that requires many 50-ledger batches.
2. Compare the current map-based validation against a version that queries `COUNT(*)`, `MIN(sequence)`, and `MAX(sequence)` for each batch before deciding whether a gap exists.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:271-287` — current gap validation allocates a temporary map and scans every expected sequence in the batch.
- `cmd/stellar-rpc/internal/db/ledger.go:307-329` — `getLedgerCountInRange` already exposes the count/min/max query shape that initially looked reusable.

## Evidence

The validation logic does extra work on every batch even though the SQL layer already has a count helper. On first inspection, replacing per-batch map allocation with a count query looked like a straightforward planner cleanup.

## Anti-Evidence

`getLedgerCountInRange` would add an extra SQL query to every batch, and it still would not identify the first missing sequence for the error message without additional logic. The current map only touches at most 50 already-deserialized ledgers in memory, so the extra DB round-trip would cost far more than the tiny allocation it removes.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Failed At**: hypothesis
**Novelty**: PASS — not previously investigated

### Why It Failed

The suspected waste is too small and the proposed replacement is more expensive than the status quo. Moving gap validation into a second SQL query would add I/O to every batch to save a tiny in-memory map over at most 50 items.

### Lesson Learned

For the post-batching `getTransactions` path, the remaining meaningful wins come from avoiding full-ledger fetch/decode work, not from micro-optimizing the 50-row validation logic that runs after the expensive query has already completed.
