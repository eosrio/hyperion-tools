---
status: accepted
date: 2026-10-17
---

# Manage each chain extension as one package with separate internal components

Operators manage one versioned chain extension package, such as a Wire package. It can provide
protocol profiles, codec providers for the relevant Node/Rust consumers, normalization rules,
and optional contract/API modules. Keep protocol adaptation and contract indexing behind their
separate interfaces even when distributed together.

The package has one operator-facing installation, upgrade, and activation workflow. Its release
identifies a compatible internal component set; tooling handles applicable runtime/platform
artifacts and compatibility checks. Operators must not separately install, version-match, or
activate the Node and Rust pieces. Validate required components before activation and identify
incomplete/incompatible packages clearly.

Provider selection must occur early enough for protocol decoding, and applicable live/bulk/offline
paths must follow the declared protocol semantics. Projection-hook authoring language does not
dictate codec implementation language. Native changes may require rebuilt artifacts and controlled
worker restarts under the existing reload policy.

Exact manifests, component ABIs, bindings/linking/IPC mechanisms, supported platform distribution,
and coordinated activation/recovery details remain open. This decision does not itself select the
4.5 Wire support baseline, require every latest Wire feature, or settle the projection-hook runtime.

Working Wire support was subsequently accepted as a mandatory 4.5 launch requirement in
[ADR 0031](0031-require-working-wire-support-for-45.md); its exact compatibility baseline remains open.

The default contract projection-hook runtime was subsequently accepted in
[ADR 0033](0033-default-javascript-typescript-projection-hooks.md), separately from protocol-codec
implementation choices.
