---
status: accepted
date: 2026-10-16
---

# Revise mappings independently within an ABI deployment

Identify a contract mapping by chain, contract, ABI deployment, and an independent immutable mapping
revision. Fixing an indexing bug or adding a derived field can therefore produce a new revision
without an on-chain ABI redeployment. Keep the ABI compatibility boundary intact and use staged
projection replacement for revisions requiring rebuilding. Revision encoding, active-version
selection, and original-versus-corrected replay policy remain to be defined.
