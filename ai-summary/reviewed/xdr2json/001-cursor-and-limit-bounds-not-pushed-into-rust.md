# H001: `processChunksJSON` Still Converts Entire Ledgers Even When the Page Needs Only a Slice

**Date**: 2026-04-07
**Subsystem**: xdr2json
**Severity**: High
**Impact**: latency / CPU / allocations
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When a JSON `getTransactions` request starts in the middle of a ledger or only has
a few response slots left for the final ledger, the Rust helper should only
extract the transactions that can actually appear in the response. It should not
serialize skipped prefix transactions or over-limit tail transactions that Go will
discard immediately.

## Mechanism

`processChunksJSON()` computes `startTxIdx` and enforces the global `limit`, but it
applies both filters **after** `xdr2json.LCMTransactionsToJSON(chunk.Lcm)` has
already parsed and serialized the entire ledger. On dense ledgers, the first
ledger can spend most of its work on prefix transactions that fail
`rtx.ApplicationOrder < startTxIdx`, and the last ledger can spend most of its
work on transactions beyond the page boundary that are dropped once `len(txns) >= limit`.
Pushing `(startTxIdx, remainingLimit)` into the FFI call would let Rust skip whole
transactions before any JSON extraction work begins.

## Trigger

1. Issue `getTransactions` with `format=json` against a dense ledger containing
   far more transactions than the requested page size.
2. Use a cursor that starts late in the first selected ledger, or a small limit
   that stops early in the last selected ledger.
3. Compare current runtime against a prototype that passes per-ledger
   `(start_index, max_items)` into Rust and only extracts the needed subset.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:251-333` — `processChunksJSON()` calls `LCMTransactionsToJSON()` before applying `startTxIdx` and `limit`.
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:42-79` — `LCMTransactionsToJSON()` currently has no way to express per-ledger bounds.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:351-377` — `lcm_transactions_to_json()` accepts only the raw LCM bytes.
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:475-556` — `extract_transactions_json()` always walks `0..count` for the full ledger.

## Evidence

The Go loop computes `startTxIdx` at lines 275-279, but the Rust call happens at
line 282 and returns all transactions for the ledger. Only afterwards does Go
skip prefix items at lines 292-295 and stop at the page limit at lines 322-325.
That means the helper currently has zero visibility into which transactions are
actually needed.

## Anti-Evidence

Full-ledger pages and very small ledgers will see little benefit because most or
all extracted transactions are returned. The fix requires a new bounded helper
signature and careful preservation of application-order numbering for the filtered
subset.

---

## Review

**Verdict**: VIABLE
**Severity**: Medium
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated. Fail/009 covered Go↔Rust LCM reparse (different code path, pre-processChunksJSON). Fail/012 covered db.Transaction transport object. Neither addresses bounding the Rust-side JSON serialization loop.

### Trace Summary

Traced the complete `processChunksJSON` path: `getTransactionsByLedgerSequence` (get_transactions.go:438-445) dispatches JSON requests to `processChunksJSON` (lines 251-333), which calls `xdr2json.LCMTransactionsToJSON(chunk.Lcm)` at line 282 for each ledger chunk. This Go function (conversion.go:45-78) pins the raw LCM bytes and calls `C.lcm_transactions_to_json` → Rust `lcm_transactions_to_json` (lib.rs:356-382) → `extract_transactions_json` (lib.rs:477-557). The Rust function parses the full LCM via `read_xdr_to_end` (line 362), then iterates `for i in 0..count` (line 508), calling `serde_json::to_value` on result, meta, and envelope for EVERY transaction (lines 526-531) and `extract_events` for EVERY transaction (line 535). Go receives the full array, unmarshals it (conversion.go:74), then filters by `startTxIdx` (get_transactions.go:292-295) and `limit` (lines 322-325), discarding any surplus.

### Code Paths Examined

- `cmd/stellar-rpc/internal/methods/get_transactions.go:275-282` — `startTxIdx` computed BEFORE Rust call, but not passed to it; Rust call serializes all transactions
- `cmd/stellar-rpc/internal/methods/get_transactions.go:292-295` — Go skips prefix transactions where `rtx.ApplicationOrder < startTxIdx` AFTER Rust already serialized them
- `cmd/stellar-rpc/internal/methods/get_transactions.go:322-325` — Go breaks at `len(txns) >= limitInt` AFTER Rust already serialized all remaining transactions
- `cmd/stellar-rpc/internal/xdr2json/conversion.go:45-78` — `LCMTransactionsToJSON` accepts only `lcmBytes []byte`, no offset/limit parameters
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:356-382` — `lcm_transactions_to_json` accepts only `CXDR`, no bounds parameters
- `cmd/stellar-rpc/lib/xdr2json/src/lib.rs:477-557` — `extract_transactions_json` loops `0..count` unconditionally; `serde_json::to_value(meta)` (line 528) is the dominant per-transaction cost for Soroban txns
- `cmd/stellar-rpc/internal/db/transaction.go:171-201` — `GetLedgerSequencesWithTransactions` uses row-level precision (LIMIT on (ledger_sequence, application_order) pairs), meaning returned ledgers may contain many more transactions than the page needs

### Findings

**The inefficiency is real and confirmed.** The Rust `extract_transactions_json` loop (lib.rs:508-554) performs `serde_json::to_value` for result, meta, and envelope on every transaction in the ledger, plus `extract_events` for each. Go then discards prefix transactions (before cursor) and suffix transactions (beyond limit). The JSON serialization — particularly `serde_json::to_value(meta)` for Soroban transactions with large `TransactionMeta` — is the dominant per-transaction Rust cost.

**Waste quantification by scenario:**
- **Dense single ledger, small limit** (e.g., 200 txns in ledger, limit=20, cursor at tx 100): Rust serializes 200 txns, Go uses 20 → 90% of Rust JSON serialization is wasted
- **First-page, small limit on dense ledger** (200 txns, limit=20, no cursor): Rust serializes 200, Go uses 20 → 90% waste (no cursor means `startTxIdx=1`, but limit still truncates)
- **Moderate pagination** (5 ledgers × 50 txns, limit=200, cursor at tx 25 in first): 24 prefix + ~25 suffix wasted = ~49 out of ~249 processed → ~20% waste
- **Sparse ledgers** (5 txns/ledger, limit=200): negligible waste

**Important caveat: XDR parsing is NOT saved.** `read_xdr_to_end` (lib.rs:362) must parse the entire LCM to produce the typed `LedgerCloseMeta` structure. Similarly, `extract_envelopes` (lib.rs:410-429) extracts all envelopes. The savings come exclusively from skipping the per-transaction JSON serialization loop (lines 508-554). Since JSON serialization dominates over XDR parsing for large Soroban transactions, this is still a substantial savings.

**Severity downgrade from High to Medium:** The hypothesis claims High (>20% latency reduction). While worst-case scenarios clearly exceed 20%, the average improvement across typical workloads is 5-20% because: (1) first-page requests without cursors only waste on the last ledger, (2) the unavoidable full-LCM XDR parse represents a non-trivial base cost, (3) sparse networks see minimal benefit. Networks with consistently dense ledgers (100+ txns) would see larger improvements, potentially reaching High territory for small-limit pagination.

### PoC Guidance

- **Target code**:
  - `cmd/stellar-rpc/lib/xdr2json/src/lib.rs`: Modify `lcm_transactions_to_json` to accept two additional `size_t` parameters: `start_index` (1-based, matching Go's `startTxIdx`) and `max_count` (0 = unlimited). Modify `extract_transactions_json` to accept `start_index: usize` and `max_count: usize`, changing the loop from `for i in 0..count` to `for i in (start_index-1).min(count)..end_index` where `end_index = if max_count > 0 { (start_index-1+max_count).min(count) } else { count }`. The `application_order` computation `(i as i32) + 1` naturally preserves correctness since `i` is still the original index.
  - `cmd/stellar-rpc/lib/xdr2json.h`: Update the `lcm_transactions_to_json` C declaration to include `size_t start_index, size_t max_count`.
  - `cmd/stellar-rpc/internal/xdr2json/conversion.go`: Update `LCMTransactionsToJSON` signature to `LCMTransactionsToJSON(lcmBytes []byte, startIndex int32, maxCount uint)` and pass these to the C call.
  - `cmd/stellar-rpc/internal/methods/get_transactions.go:282`: Pass `startTxIdx` and the remaining limit (`uint(limitInt - len(txns))`) to the updated `LCMTransactionsToJSON`.
- **Change description**: Push pagination bounds into the Rust FFI so `extract_transactions_json` only serializes the transactions Go will actually use. The Rust loop skips `serde_json::to_value` and `extract_events` for out-of-bounds transactions. Go-side filtering (lines 292-295 and 322-325) can be simplified but should be kept as a safety check.
- **Correctness check**: Existing `getTransactions` integration tests cover cursor-based pagination and limit enforcement. Run `make go-test` to verify all tests pass. Also verify that `application_order` values in the response remain correct (1-based, matching original ledger ordering, not filtered-subset ordering).
- **Benchmark focus**: Measure `getTransactions` with `format=json`, limit=20, on ledgers with 100+ transactions, with and without cursor. Primary metric: p50/p99 latency reduction. Expected improvement: 20-60% reduction in Rust-side processing time for dense-ledger + small-limit scenarios, translating to 10-40% end-to-end latency reduction depending on how much of total request time is Rust JSON serialization.
