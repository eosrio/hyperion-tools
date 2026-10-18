---
status: accepted
date: 2026-10-16
---

# Generate state mappings automatically for allowed accounts

Full auto-map mode lets operators allow contract accounts and rely on Hyperion to build and maintain
their state mappings from deployed ABIs. ABI deployments trigger compatibility validation and mapping
generation/update without requiring operators to maintain individual table definitions. Generated
mappings follow the same ABI binding, immutable revision, backend portability, and staged activation
contracts as authored definitions. Keep specialized domain behavior in definitions/hooks and isolate
contract failures without halting chain indexing. Physical index policy remains a separate decision.
