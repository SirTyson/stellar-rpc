# H002: By-Sequence Ledger Fetch Keeps Raw Blobs and Re-Decodes Full `LedgerCloseMeta` Values in the Handler

**Date**: 2026-04-07
**Subsystem**: methods
**Severity**: Medium
**Impact**: latency / allocations / GC pressure
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

Once `getTransactions` has already selected the exact ledger sequences it needs, each fetched row should be XDR-decoded once and carried through the hot path as a typed `xdr.LedgerCloseMeta`. Presence validation should use the SQL `sequence` column directly rather than reparsing raw blobs and then asking the handler to unmarshal those same blobs again.

## Mechanism

`BatchGetLedgersBySequences()` currently scans `meta` into `[][]byte`, partially parses each blob just to recover the ledger sequence, and then `getTransactionsByLedgerSequence()` calls `lcm.UnmarshalBinary(chunk.Lcm)` before transaction processing. That leaves every selected ledger paying for a SQLite blob copy, temporary raw-blob retention, header parsing in the DB helper, and a second full `LedgerCloseMeta` decode in the handler. The codebase already proves a cheaper pattern is available: `getTransactionByHash()` scans `meta` directly into a struct field of type `xdr.LedgerCloseMeta`, so a `BatchGetLedgerMetasBySequences()` helper that selects `sequence, meta` should be able to remove the raw-blob handoff and the second full decode.

## Trigger

1. Use `getTransactions` on a sparse window where the planner correctly selects many non-empty ledgers that are all actually needed to fill the page.
2. Profile allocations and CPU in the `BatchGetLedgersBySequences` -> `UnmarshalBinary` path.
3. Compare against a version that scans `SELECT sequence, meta` directly into typed `xdr.LedgerCloseMeta` rows and validates presence from the SQL sequence column.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:315-349` — the handler receives raw `chunk.Lcm` blobs and fully unmarshals each ledger before processing.
- `cmd/stellar-rpc/internal/db/ledger.go:162-204` — `BatchGetLedgersBySequences()` materializes `[][]byte` and partially reparses each blob.
- `cmd/stellar-rpc/internal/db/transaction.go:227-245` — `getTransactionByHash()` is existing prior art for scanning `meta` directly into `xdr.LedgerCloseMeta`.

## Evidence

The by-sequence helper is unique in forcing the handler to own the full `UnmarshalBinary()` step after the DB layer has already touched the same blob. In contrast, the existing single-transaction lookup shows that `support/db.Select` can populate typed XDR fields directly inside Go structs. That makes the current raw-blob handoff look like an avoidable compatibility artifact rather than a hard requirement of the DB stack.

## Anti-Evidence

This does not reduce how many ledgers the planner selects, so dense-page overfetch remains the larger problem when selection is too broad. The win therefore concentrates on workloads where the selected ledgers are genuinely needed and the repeated decode/copy overhead is a meaningful fraction of total request time.

---

## Review

**Verdict**: NOT_VIABLE
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated (related to H008 but proposes opposite direction)
**Failed At**: reviewer

### Trace Summary

Traced the full code path from `getTransactionsByLedgerSequence` (get_transactions.go:314-359) through `BatchGetLedgersBySequences` (ledger.go:162-204) and the handler's `UnmarshalBinary` call (get_transactions.go:344-345). The hypothesis claims a "second full `LedgerCloseMeta` decode" but this is factually wrong: the DB layer performs only a **partial** decode (version discriminant + optional extension + header — ~20µs) while the handler performs the single **full** decode via `UnmarshalBinary`. Confirmed via `xdr/db.go` that `LedgerCloseMeta.Scan()` simply calls `UnmarshalBinary(src.([]byte))`, so the proposed `db.Select` into `[]xdr.LedgerCloseMeta` would perform the exact same full decode — just in the DB layer instead of the handler. Total decode work is unchanged.

### Code Paths Examined

- `cmd/stellar-rpc/internal/db/ledger.go:162-204` — `BatchGetLedgersBySequences` scans into `[][]byte`, then for each blob: reads `xdr.Int32` (4 bytes), optionally skips `LedgerCloseMetaExt`, unmarshals `LedgerHeaderHistoryEntry` (~400 bytes). This is a **partial** decode costing ~7-20µs per ledger — NOT a full decode.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:339-359` — handler loop calls `lcm.UnmarshalBinary(chunk.Lcm)` for each chunk. This is the **only** full decode, costing ~100µs-10ms per ledger depending on transaction density. The loop can `break` early if the page fills before all chunks are processed.
- `go-stellar-sdk@v0.4.0/xdr/db.go:Scan` — `LedgerCloseMeta.Scan(src)` calls `l.UnmarshalBinary(src.([]byte))`. This confirms that `db.Select` into `[]xdr.LedgerCloseMeta` would perform identical full decode work, just at a different call site.
- `cmd/stellar-rpc/internal/db/ledger.go:138-160` — `BatchGetLedgerMetas` (contiguous range variant) already uses `db.Select` into `[]xdr.LedgerCloseMeta`. The pattern exists but produces no fewer decodes.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:323-337` — verification step uses `chunk.Header.Header.LedgerSeq` from the partial decode. This check happens BEFORE any full decode, allowing early error detection without paying full decode cost.

### Why It Failed

The hypothesis's central claim — "a second full `LedgerCloseMeta` decode in the handler" — is wrong. There is only ONE full decode per ledger (in the handler at line 345). The DB layer does a partial header-only decode costing ~0.1-2% of the full decode. The proposed fix (scanning directly into `[]xdr.LedgerCloseMeta`) would:

1. **Eliminate the partial decode**: Saves ~7-20µs per ledger, or ~0.1-2% of the full decode cost. For a page touching 10 ledgers, this is ~70-200µs out of a 5-50ms request — well under 1%.

2. **Force upfront full decode of ALL fetched LCMs**: Currently, the handler decodes LCMs sequentially and can `break` early when the page fills. With `db.Select` into `[]xdr.LedgerCloseMeta`, ALL LCMs are fully decoded before the handler sees any of them, losing early-termination savings.

3. **Increase peak memory**: Current approach holds N raw blobs (~500KB each) + 1 decoded LCM at a time. Proposed approach holds N fully decoded LCMs simultaneously. Go struct representations are typically **larger** than raw XDR (pointer indirection, slice headers, string headers), so peak memory would increase.

4. **Mirror of H008**: H008 (fail/methods/008) proposed the opposite direction (switching from full decode to partial decode in the DB layer) and was rejected for the same fundamental reason: reorganizing WHERE the decode happens doesn't reduce total decode work. H002 proposes the reverse direction and fails for the same reason.

### Lesson Learned

When evaluating "double decode" claims, distinguish between partial decodes (cheap header extraction) and full decodes. A partial decode followed by a full decode is NOT equivalent to two full decodes — the partial decode costs 0.1-2% of the full decode and provides useful header data for verification before committing to the expensive full decode. The codebase's choice to use `LedgerMetadataChunk` (raw bytes + header) instead of fully decoded `LedgerCloseMeta` for batch fetches is a deliberate design that enables early verification and sequential processing with bounded memory.
