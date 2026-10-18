---
status: accepted
date: 2026-10-16
---

# Bind contract mapping versions to ABI deployments and stage projection replacements

Contract mappings are versioned by the contract's on-chain ABI deployment so historical indexing
uses the appropriate mapping for each deployment. When a mapping change needs a rebuilt projection,
keep the previous version serving while the replacement is built, caught up, and validated, then
coordinate the switch. An existing mapping may process only ABI versions it correctly supports;
serving an older projection does not make it current after an incompatible ABI change. Independent
mapping revisions within a deployment are defined in ADR 0010; detailed activation contracts remain open.
