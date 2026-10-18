---
status: accepted
date: 2026-10-17
---

# Default to retryable unavailable when a required projection fails

When a contract-specific failure leaves a required projection unable to satisfy an endpoint's
serving contract, return a retryable unavailable error by default. Apply this to the affected query
scope; unrelated queries and indexing continue.

A plugin may explicitly support serving a preserved last-valid view. Such serving must satisfy the
endpoint's declared contract and the mandatory served-view block number and shared HTTP metadata
requirements in [ADR 0036](0036-require-block-number-for-last-valid-state.md) and
[ADR 0037](0037-shared-http-state-metadata-headers.md). A checkpoint alone does not reconstruct an
earlier valid view after partial writes.

Ordinary processing lag, compatible generations serving during staged rebuilds, and reference API
semantics allowing optional missing data retain their accepted behavior. Last-valid-state support
cannot bypass the query pause for known orphaned state during fork recovery.

Exact retryable error representations, endpoint declarations, and staleness limits remain to be
specified through the shared interfaces and each API's compatibility contract.
