---
status: accepted
date: 2026-10-16
---

# Require full AtomicAssets and AtomicMarket API parity for Hyperion 4.5

Hyperion 4.5 must provide full API parity for both AtomicAssets and AtomicMarket. Partial coverage
does not satisfy the release requirement: missing behavior would force operators to retain their
separate serving stack and prevent the intended consolidation.

This requirement also applies when 4.5 launches with MongoDB alone under ADR 0007. WormDB readiness
may change which state backends ship, but it does not reduce the required API surface. Any backend
offered as supported for this surface must meet the same client-visible compatibility contract.

The reference API is pinned to release 2.7.0 and commit
`0aabc47dcf8bd0804d4dcecb336c90fc02a9c89e` under
[ADR 0014](0014-pin-atomic-api-270-parity-baseline.md). Contract deployment/version coverage,
the required interfaces and detailed behaviors, and conformance evidence remain to be specified.
A route checklist alone is insufficient to establish behavioral parity.
