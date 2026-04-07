# H001: `getTransactions` JSON Mode Still Repackages Each Transaction Through `db.Transaction`

**Date**: 2026-04-07
**Subsystem**: xdr2json
**Severity**: Medium
**Impact**: latency / CPU / allocations / GC
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

The JSON `getTransactions` path should derive transaction metadata once and convert
the typed `ingest.LedgerTransaction` directly into the final JSON fragments. It
should not first repack the same transaction into a transient `db.Transaction`
containing freshly allocated XDR byte slices that are consumed immediately by
xdr2json and then discarded.

## Mechanism

`processTransactionsInLedger()` already extracts `txHash`, `applicationOrder`,
`feeBump`, `ledger`, and `createdAt` directly from `ingestTx`, then the JSON
branch calls `db.ParseTransaction()` which recomputes the same metadata and
materializes `Result`, `Meta`, `Envelope`, and event XDR byte slices into a
transport struct. A JSON-only fast path that keeps the typed `ingestTx` in hand
and uses a reusable `xdr.EncodingBuffer` scratch buffer for the xdr2json calls
would remove duplicate metadata work plus one layer of per-field/per-event heap
allocation before Rust starts parsing.

## Trigger

1. Issue `getTransactions` with `xdrFormat=json` on pages containing many
   transactions, especially Soroban transactions with large metas and events.
2. Capture allocation profiles around `processTransactionsInLedger`,
   `db.ParseTransaction`, and `xdr.EncodingBuffer.MarshalBinary`.
3. Compare against a prototype that replaces `db.ParseTransaction()` +
   `transactionToJSON(tx)` with a JSON helper that consumes `ingestTx`
   directly and reuses one `EncodingBuffer` per request.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:processTransactionsInLedger:141-182` — JSON mode already has the typed `ingestTx` and fills much of `txInfo` before calling `db.ParseTransaction()`.
- `cmd/stellar-rpc/internal/db/transaction.go:ParseTransaction:262-301` — repacks the same transaction into `db.Transaction` for the JSON path.
- `cmd/stellar-rpc/internal/db/transaction.go:parseEvents:304-340` — allocates event XDR slices solely so later code can hand them to xdr2json.
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/xdr/main.go:EncodingBuffer.UnsafeMarshalBinary/MarshalBinary:188-227` — existing reusable scratch-buffer API already avoids exactly this class of per-call allocation on the XDR path.

## Evidence

The XDR branch in `processTransactionsInLedger()` already uses one request-scoped
`xdr.EncodingBuffer` and converts directly from typed values, while the JSON
branch detours through `db.Transaction` and allocates new byte slices for fields
that are not stored or reused anywhere after the immediate xdr2json calls. The
duplication is especially visible because `txInfo` is partially populated from
`ingestTx` before `ParseTransaction()` recomputes overlapping state.

## Anti-Evidence

The fast path still needs to serialize typed Go XDR objects back to bytes because
xdr2json only consumes XDR bytes today, so it cannot remove the actual XDR encode
step entirely. The improvement depends on allocation pressure from the transient
transport object being large enough to matter alongside Rust-side XDR parsing and
JSON serialization.
