---
status: accepted
date: 2026-10-17
---

# Require working Wire protocol-extension support for Hyperion 4.5

Hyperion 4.5 must launch with working Wire support through the unified chain extension package
defined in ADR 0030. It is the concrete acceptance case for the protocol extension interfaces,
including applicable live ingestion and historical replay against an agreed, pinned node/protocol
baseline. Interface definitions and isolated source/decoder examples alone do not satisfy this
requirement.

Operators manage one package whose internal components cover the relevant Node/Rust paths.
The agreed indexing, finality, fork-recovery, durability, and backend-interface contracts remain
applicable. A MongoDB-only launch does not waive working Wire support.

The exact node/codec revisions, supported protocol features, fixtures, end-to-end
scenarios, and required runtime/backend combinations remain to be agreed. Investigation commits
are not automatically the support baseline, and upstream development does not silently expand it.
This decision concerns protocol compatibility and does not define additional client API parity.

The network target was subsequently accepted in
[ADR 0032](0032-qualify-wire-on-new-testnet-with-wire-contracts.md): a new Wire testnet launched with
Wire's contracts. Its exact node/contract/extension pins remain to be selected.

Implementation and qualification are outstanding. The source investigation is evidence for the
architecture, not acceptance that the working-plugin requirement has been achieved.
