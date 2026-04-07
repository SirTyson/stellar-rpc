# H003: Batched `getTransactions` replies accumulate and recopy the entire payload before write-out

**Date**: 2026-04-07
**Subsystem**: network
**Severity**: Medium
**Impact**: batch heap pressure / copy amplification
**Hypothesis by**: gpt-5.4, high

## Expected Behavior

A JSON-RPC batch containing multiple large `getTransactions` calls should not need to keep every member response envelope live and then duplicate all of them into one giant aggregate slice before the HTTP timeout layer copies the batch again. Batch assembly should avoid a full second pass over the total batch payload.

## Mechanism

For batch requests, `serveInternal` appends each fully materialized member response to `results`, and `directBridgeJoinBatch` then allocates a new aggregate buffer and copies every `results[i]` into `[ ... ]`. With batched JSON-format `getTransactions`, each member can already be megabytes long, so transient live bytes become `sum(member envelopes)` plus the joined batch plus the HTTP timeout buffer. That copy amplification can turn one batched request into a large GC event and materially increase tail latency or reduce sustainable RPS for batch clients.

## Trigger

Send JSON-RPC batch requests containing 4-16 `getTransactions` calls with `xdrFormat=json` and `pagination.limit=200`, and compare baseline against a version that streams batch assembly or constructs the final batch buffer once without retaining and re-copying each full member envelope.

## Target Code

- `cmd/stellar-rpc/internal/directbridge.go:69-123` — `serveInternal` retains every member response in `results`
- `cmd/stellar-rpc/internal/directbridge.go:189-207` — `directBridgeJoinBatch` allocates a second buffer and copies all member envelopes into it
- `cmd/stellar-rpc/internal/network/requestdurationlimiter.go:88-90` — the HTTP timeout wrapper adds another full copy of the joined batch
- `/home/garand/go/pkg/mod/github.com/stellar/go-stellar-sdk@v0.4.0/protocols/rpc/get_transactions.go:74-90` — each batch member can contain many large raw JSON transaction fields

## Evidence

The current batch path explicitly stores each member as a separate `json.RawMessage` and then concatenates those messages into a new array buffer. Because `getTransactions` responses are large, the extra aggregate copy is proportional to the total batch payload, not to a small metadata header.

## Anti-Evidence

Single-request `getTransactions` traffic sees no benefit from this optimization, so the impact depends on clients actually using JSON-RPC batch. If production callers almost never batch `getTransactions`, this may have little real-world effect despite being mechanically valid.
