# H003: JSON `getTransactions` Builds a Short-Lived Byte Graph Only to Walk It Once

**Date**: 2026-04-07
**Subsystem**: methods
**Severity**: Low
**Impact**: CPU / allocations / GC
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

For `format=json`, the handler should convert each returned transaction with minimal intermediate heap state. It should not first build a byte-oriented `db.Transaction` object containing result/meta/envelope/event buffers when those buffers are consumed exactly once and discarded immediately after JSON conversion.

## Mechanism

`processTransactionsInLedger` always calls `db.ParseTransaction` before the format switch. `ParseTransaction` eagerly marshals `Result`, `Meta`, `Envelope`, `DiagnosticEvents`, `TransactionEvents`, and `ContractEvents` into nested byte slices stored on `db.Transaction`; the JSON branch then immediately traverses that object again via `transactionToJSON`, `jsonifySlice`, and `BuildEventsJSONFromTransaction`. A JSON-specific builder that marshals each XDR value directly into the `xdr2json` batch helpers and response fields could remove the short-lived `db.Transaction` byte graph and cut allocation/GC pressure on event-heavy JSON pages.

## Trigger

1. Issue `getTransactions` with `format=json` against ledgers containing many diagnostic, transaction, and contract events.
2. Capture allocation profiles and retained heap during a 50-200 transaction page.
3. Compare against a version that bypasses `db.Transaction` for the JSON path and streams each marshaled item directly into JSON conversion/output assembly.

## Target Code

- `cmd/stellar-rpc/internal/methods/get_transactions.go:129-176` — the handler calls `db.ParseTransaction` before selecting the JSON path.
- `cmd/stellar-rpc/internal/db/transaction.go:258-319` — `ParseTransaction` eagerly marshals every returned field and event family into byte slices on `db.Transaction`.
- `cmd/stellar-rpc/internal/methods/json.go:12-90` — the JSON helpers immediately walk those byte slices again for conversion.
- `cmd/stellar-rpc/internal/methods/get_transaction.go:143-155` — event JSON conversion consumes the already-marshaled slices rather than typed XDR values.

## Evidence

The `db.Transaction` type is byte-backed by design (`[]byte`, `[][]byte`, `[][][]byte` fields), and the JSON branch never uses the XDR/base64 helpers that motivated that representation. In the hot path, the object exists only long enough to feed `xdr2json`, which makes it a clear candidate for a format-specific fast path that trades one generic intermediate representation for direct JSON assembly.

## Anti-Evidence

`xdr2json` still ultimately needs XDR bytes, so this optimization removes heap churn more than it removes the underlying marshaling work. The gain is therefore JSON-only and likely smaller than planner/read-path fixes that cut entire ledger fetches or decodes.
