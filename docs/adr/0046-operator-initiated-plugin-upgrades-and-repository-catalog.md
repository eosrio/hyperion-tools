---
status: accepted
date: 2026-10-17
---

# Use operator-initiated plugin upgrades and an official repository catalog

Executable plugin upgrades are initiated by the operator, with Hyperion selecting tested compatible
versions under the accepted release policy. Compatibility metadata changes do not automatically
replace installed or running plugin code. A standard Hyperion distribution upgrade can deliver its
pinned plugins as part of the operator-initiated core upgrade.

EOS Rio provides a repository as the official plugin catalog, where plugin compatibility is
declared. This is a repository catalog, without a new hosted registry/marketplace requirement.
Operators remain responsible for auditing their chosen plugins under
[ADR 0045](0045-trusted-plugins-and-operator-audit-responsibility.md).

Independent plugin releases, tested compatible release pins, and bundled plugins disabled by
default retain their accepted contracts. Automatic ABI-driven mappings within allowed accounts,
live declarative mapping reload, and required indexing dependency activation are distinct from
executable code installation/upgrades and do not authorize automatic code replacement.

Exact repository location, catalog schema, declaration provenance/testing rules, package references,
SDK/interface compatibility fields, and upgrade tooling remain to be specified. The SDK versioning
policy was subsequently accepted in
[ADR 0047](0047-independent-sdk-versioning-and-major-compatibility.md).
