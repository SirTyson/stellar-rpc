# H004: `BatchGetLedgersBySequences` Decodes Every Selected Ledger Header Twice

**Date**: 2026-04-07
**Subsystem**: methods
**Severity**: Low
**Impact**: CPU / allocations
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

When `getTransactions` fetches a non-contiguous set of ledgers, the DB helper should return the ledger sequence number directly from SQL and let the caller decode each `LedgerCloseMeta` exactly once. It should not partially unmarshal every blob just to recover a sequence number that is already stored in the `sequence` column.

## Mechanism

`BatchGetLedgersBySequences()` selects only `meta`, then partially XDR-decodes version, extension, and header for every row so it can populate `chunk.Header.Header.LedgerSeq` for the caller's completeness check. The handler later fully unmarshals `chunk.Lcm` before transaction processing. Changing the query to return `sequence, meta` would let the missing-ledger verification use the SQL column directly, removing the extra `bytes.Reader` creation and XDR decode on every fetched ledger.

## Trigger

1. Use a request that selects many non-empty ledgers through the transaction index (for example, `limit=200` on a sparse window with one transaction per ledger).
2. Profile CPU samples and allocations in the current indexed path.
3. Compare against a version of `BatchGetLedgersBySequences()` that selects `sequence` alongside `meta` and skips the partial header parse.

## Target Code

- `cmd/stellar-rpc/internal/db/ledger.go:BatchGetLedgersBySequences:164-204` — fetches only `meta` and partially XDR-decodes each blob to recover the ledger header.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:323-337` — uses that partially decoded header only to build a sequence-presence map.
- `cmd/stellar-rpc/internal/methods/get_transactions.go:344-349` — fully unmarshals the selected blob again before transaction processing.

## Evidence

The helper's partial decode is not needed for response formatting; it exists only because the SQL result omits `sequence`. The sequence is already indexed and available as a normal table column, so paying an XDR parse per selected ledger just to rebuild that information is a byproduct of the current query shape rather than a requirement of the endpoint.

## Anti-Evidence

If the request selects only one or two ledgers, the extra decode is tiny. Full ledger unmarshal, envelope hashing, and transaction serialization still dominate end-to-end cost on dense ledgers, so this is most plausible as a low-severity follow-on optimization or as a multiplier on top of the dense overfetch issue above.

---

## Review

**Verdict**: VIABLE
**Severity**: Low
**Date**: 2026-04-07
**Reviewed by**: claude-opus-4-6, high
**Novelty**: PASS — not previously investigated

### Trace Summary

`BatchGetLedgersBySequences` (ledger.go:164-204) is called exclusively by `getTransactions` (get_transactions.go:315) with sequence numbers already obtained from the transaction index. For each fetched blob, it creates a `bytes.NewReader`, partially XDR-decodes the version discriminant (Int32), the `LedgerCloseMetaExt` (to skip past it), and the full `LedgerHeaderHistoryEntry` (~300-400 bytes of reflected XDR). The caller then only uses `chunk.Header.Header.LedgerSeq` for a presence check (line 327) and an error message (line 348), before performing a complete `UnmarshalBinary` on the same blob (line 345) which re-decodes the header along with the entire LCM.

### Code Paths Examined

- `cmd/stellar-rpc/internal/db/ledger.go:164-204` — `BatchGetLedgersBySequences` selects only `meta`, allocates `bytes.NewReader` per blob, and does three `xdr.Unmarshal` calls (Int32, LedgerCloseMetaExt, LedgerHeaderHistoryEntry) to populate `chunk.Header`
- `cmd/stellar-rpc/internal/db/ledger.go:87-90` — `LedgerMetadataChunk` struct has `Header xdr.LedgerHeaderHistoryEntry` and `Lcm []byte`
- `cmd/stellar-rpc/internal/methods/get_transactions.go:304-306` — `ledgerSeqs` is already known from `GetLedgerSequencesWithTransactions`
- `cmd/stellar-rpc/internal/methods/get_transactions.go:323-337` — presence check only uses `c.Header.Header.LedgerSeq`, i.e., the sequence number
- `cmd/stellar-rpc/internal/methods/get_transactions.go:344-349` — full `UnmarshalBinary(chunk.Lcm)` re-decodes the entire blob including the same header bytes
- `cmd/stellar-rpc/internal/methods/json.go:40-55` — `ledgerToJSON` uses `chunk.Header` extensively but is only called from `get_ledgers.go:277`, NOT from `get_transactions.go`
- `cmd/stellar-rpc/internal/methods/get_ledgers.go:266-270` — `get_ledgers.go` uses `chunk.Header` for sequence, close time, and JSON conversion, but it calls `BatchGetLedgers`, not `BatchGetLedgersBySequences`

### Findings

The inefficiency is confirmed. `BatchGetLedgersBySequences` is used exclusively by `getTransactions`, and `getTransactions` never accesses `chunk.Header` beyond `Header.Header.LedgerSeq`. The sequence number is already known to the caller (it's in `ledgerSeqs`, passed as input to the function) and is available as a SQL column.

Per-ledger redundant work:
- 1 `bytes.NewReader` allocation
- `xdr.Unmarshal` of `Int32` (4 bytes, reflection overhead)
- `xdr.Unmarshal` of `LedgerCloseMetaExt` (typically 4 bytes for v=0 discriminant)
- `xdr.Unmarshal` of `LedgerHeaderHistoryEntry` (~300-400 bytes, ~15-20 reflected field decodes)

For a typical `limit=200` request fetching ~200 ledgers, this is ~200 `bytes.NewReader` allocations and ~600 reflection-based `xdr.Unmarshal` calls producing ~80KB of redundant XDR parsing. Estimated at ~2-5μs per ledger, the total is ~0.4-1.0ms of redundant CPU — approximately 3-7% of the current ~7.2ms/op benchmark for sparse scans.

No existing optimization covers this: there is no pooling or caching of the partial decode, and the full `UnmarshalBinary` at line 345 decodes the same header bytes independently.

The fix does not affect `BatchGetLedgers` (used by `getLedgers`), which legitimately needs the partially decoded header for response formatting.

### PoC Guidance

- **Target code**: `cmd/stellar-rpc/internal/db/ledger.go:BatchGetLedgersBySequences` (lines 164-204) and `cmd/stellar-rpc/internal/methods/get_transactions.go` (lines 323-348)
- **Change description**: (1) Change the SQL query in `BatchGetLedgersBySequences` to `SELECT sequence, meta`. (2) Add a `Sequence uint32` field to `LedgerMetadataChunk` (or use a local intermediate struct). (3) Populate `Sequence` from the SQL column and skip the partial XDR decode loop entirely. (4) Update `get_transactions.go:327` to use `c.Sequence` instead of `uint32(c.Header.Header.LedgerSeq)` and `get_transactions.go:348` similarly. Alternatively, since the caller already has `ledgerSeqs` and results are `ORDER BY sequence ASC`, the presence check could diff the two sorted lists without any sequence field at all.
- **Correctness check**: Existing `getTransactions` unit tests in `get_transactions_test.go` cover normal, sparse, and edge-case pagination. Run `make go-test` to verify no regressions.
- **Benchmark focus**: Run the existing `BenchmarkGetTransactionsSparseScan` benchmark. Expect a ~3-7% reduction in ns/op and a small reduction in allocs/op (~200-400 fewer allocations). The improvement will be more visible in allocation profiling (`-benchmem`) than in wall-clock time.
