---
status: accepted
date: 2026-10-15
---

# Materialize history-derived fields required by state queries

The selected state backend may store explicitly owned, rebuildable history-derived query fields and
indexes, such as transfer timestamps and true mint ordinals. Filtering, sorting, and page selection
must use the required values before pagination; later hydration cannot repair an incorrectly selected
page. Full lifecycle history stays in the history/archive tiers. This replaces a literal prohibition
on repeating any historical value with explicit projection ownership and rebuilding contracts.
