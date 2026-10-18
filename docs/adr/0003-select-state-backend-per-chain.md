---
status: accepted
date: 2026-10-15
---

# Select one state backend per chain

Operators choose MongoDB or WormDB once per chain, and all enabled state-query domains on that chain
use the selected backend. This supersedes ADR 0002's per-domain selection: overlapping state data and
cross-backend reads would introduce complexity and edge cases the operator chose to avoid. Different
chains may use different backends; both remain permanently supported alternatives.
