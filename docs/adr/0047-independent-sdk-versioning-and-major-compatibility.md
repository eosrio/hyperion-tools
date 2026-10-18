---
status: accepted
date: 2026-10-17
---

# Version the shared plugin SDK independently and preserve compatibility within each major

Version the shared plugin SDK contract independently of Hyperion core and individual plugins.
Keep changes backward compatible within each SDK major version. Breaking interface or semantic
changes require a new SDK major.

Plugins declare the SDK/interface versions they target; core declares those it supports. Reflect
those requirements in plugin compatibility metadata. Capability validation and tested first-party
release pins still apply; a version declaration alone does not prove parity or runtime correctness.

The initial stable major, migration from the existing `@eosrio/hyperion-plugin-core` package,
supported-version lifetimes, and exact manifest fields remain to be defined. Native protocol-provider
and module ABI compatibility retains its separate contract.
