# Wire implementation delivery plan

Status: Active implementation frontier, 2026-10-19. This plan applies ADRs 0005 and 0030–0032;
it does not change the mandatory working-Wire release gate or select final testnet launch pins.
Preparation and qualification remain [tools #22](https://github.com/eosrio/hyperion-tools/issues/22),
coordinated through [release #24](https://github.com/eosrio/hyperion-tools/issues/24).

## Current implementation and evidence

The initial Wire repository was a scaffold at `5218f0a`. Three investigations traced upstream
protocol differences, existing Node/SDK integration and Rust ingestion/offline paths. The immediate
implementation is [Wire #1](https://github.com/eosrio/hyperion-wire-plugin/issues/1): package-local
timestamp conversion and complete primary/secondary KV identities with preserved provenance.
It includes Node and Rust components and a shared source-derived engineering fixture corpus.

[Wire PR #4](https://github.com/eosrio/hyperion-wire-plugin/pull/4) implements that slice.
Local validation passes 327 emitted Node 24 tests, locked Rust checks, all 314 shared cross-language
vectors, deterministic fixture/hash verification and packed-content/consumer tests. Independent
Standards and Spec reviews found no remaining findings. Node 22/24 and Rust 1.88 validation runs
in the PR's CI; these checks establish primitive conformance, not complete protocol support.

The operator requested beginning node setup before selecting final launch pins. That work is
[tools #42](https://github.com/eosrio/hyperion-tools/issues/42): an isolated Wire `nodeop`
(nodeos equivalent), with owned binaries/config/data and dedicated loopback ports on an inspected
Hyperion host. Official `v1.0.0` at `7dede884e0150d36fa788e85a119e10653ade8ca` is the initial
artifact candidate; its source manifest and checksums must match before execution. This candidate
is distinct from the newer source-derived primitive profile and is not an accepted launch pin.

This first slice receives already-decoded fields. It does not implement SHiP frame decoding,
binary ABI/value codecs, ABI-deployment resolution, protocol loading, native distribution or live
fork recovery. A source bundle or passing fixture suite does not satisfy working Wire support.
Existing worker, retained-history and backend-neutral contracts remain unchanged.

## Executable frontier

| Delivery | Owner and issue | Verifiable result | Actual dependencies |
|---|---|---|---|
| Timestamp and KV primitives | [Wire #1](https://github.com/eosrio/hyperion-wire-plugin/issues/1) | Node/Rust agree on the 2025 epoch, slot bounds and all primary/secondary identity bytes, with complete payer/source/deployment provenance. Packed Node exports and internal Rust source share one profile and fixtures. | None. This implementation uses engineering fixtures and does not wait for testnet launch. |
| Complete Wire protocol/contract codecs | [Wire #2](https://github.com/eosrio/hyperion-wire-plugin/issues/2) | Pinned block/trace/ABI/KV bytes decode without losing metadata; enums, protobuf, slug values, nested timestamps, deployment changes and malformed inputs have executable oracles. | #1 supplies shared primitives. Source/build fixture preparation can proceed immediately. |
| Existing Node worker integration and additive SDK seam | [History API #212](https://github.com/eosrio/hyperion-history-api/issues/212) | A chain selects its provider before importing/constructing affected codecs; production reader/deserializer/output paths process the Wire fixtures while default Antelope behavior and retained history remain usable. | Complete #2 fixture for Wire acceptance. Generic default-provider extraction and SDK contract design can start in parallel. |
| Rust live/bulk/offline integration | [tools #41](https://github.com/eosrio/hyperion-tools/issues/41) | Applicable Rust paths select the same protocol semantics before fast-path decoding and produce equivalent output to Node. | Complete #2 fixtures. Independent of Node worker implementation; profile/fast-path extraction and Antelope regressions can start earlier. |
| Coordinated operator package and integrated qualification | [Wire #3](https://github.com/eosrio/hyperion-wire-plugin/issues/3) | One compatible internal component set installs/preflights/activates together, then passes selected-testnet live/replay and recovery exercises with retained evidence. | Node and Rust integration; selected launch inputs and provisioned testnet under tools #22. Inventory/build preparation can start earlier. |

Only #1 is immediately implementation-ready in full. Later rows expose concrete prerequisites;
they are not waiting for a speculative complete SDK. Each row owns its implementation PRs in
the repository containing the change, with SDK changes linked from the integration issue.
Cross-repository dependencies are recorded natively where supported and in issue bodies.

The next coding priority after #1 is complete ABI/value fixtures and codecs in #2, alongside the
generic provider-selection seam and Rust fast-path extraction. Final testnet availability blocks
live qualification, not source-derived codec development. Existing profiling/recovery issues remain
open with their original acceptance requirements; prioritizing Wire does not close them.

## Why provider selection must precede decoding

The current Node worker imports/constructs the legacy native codec before downstream contract
plugins initialize. The parser combines JavaScript and native decoding, while unknown delta kinds
can be skipped before normalization. A decoded action/delta hook cannot repair those earlier choices.
Keep legacy contract plugin loading separate from protocol-provider loading, make the default
Antelope implementation lazy, and pass one validated profile identity to participating workers.
Do not expose native codec classes or backend-native queries as the shared SDK interface.

Rust similarly has separate hardcoded timestamp paths, concrete binary ABI decoding and manually
constructed SHiP variants/fast paths. An archive ABI override changes advertised schema; it does
not automatically replace those codecs or configure every offline consumer. One profile must cover
both top-level and nested ABI timestamps before the path is considered compatible.

Preserve legacy Elasticsearch identifiers and mappings. New KV records need complete logical
identity and a storage-appropriate physical key policy; a long compound key cannot be truncated
or blindly used as an Elasticsearch document ID. ABI/deployment binding and canonical source
provenance remain the host's responsibility.

## Engineering fixture profile and launch inputs

| Input | Current status |
|---|---|
| Wire node source for engineering fixtures | `Wire-Network/wire-sysio` at `6508c9e8f4044df8d5acb082a7af3be6a80cd9a0`; source pin only. |
| Wire Hyperion extraction reference | `87b5e2e9647e0219fcf77d07b653668601e0ade7`; inspect extension points, do not port it wholesale or use it as the correctness oracle. |
| CDT source-inspection reference | `187d6a24ddc60c5c3f4238271815c124bcc80f11`; not yet an accepted compiler/build dependency. |
| Contract fixture inventory | `test_kv_map`, `test_kv_sec_query`, `proto_abi_test`, and representative `sysio.token`, `sysio.chains`/`sysio.opreg`, `sysio.roa` behaviors. |
| Testnet launch node/CDT builds | Pending operator selection and reproducible build inputs. Investigation revisions are not launch pins. |
| Deployed contracts and WASM/ABI artifacts | Pending inventory, build receipts and SHA-256 digests. A committed WASM beside C++ does not prove a matching build. |
| Genesis, chain identity/configuration and deployment sequence | Pending; record before node-captured qualification fixtures are accepted. |
| Extension/SDK/host/Rust artifact versions and platform set | Pending tested coordinated set and selected packaging mechanism. No catalog support claim yet. |
| Isolated node setup | Requested by the operator under tools #42; Hyperion-1 inspected through Chronos and SSH. Preserve all existing binaries/services. Official pinned release artifacts are a candidate for this engineering baseline. |
| Final testnet provisioning | Pending selected inputs, contract deployment and qualification. The isolated node setup can begin earlier; neither setup nor prior server-test authorization permits production service replacement. |

The package profile and corpus explicitly label themselves as engineering fixtures. Track each
fixture's origin as synthetic/source-derived, regenerated with exact build inputs, or captured from
the selected node/network. These are different forms of evidence.

## Concrete protocol regressions to preserve

- Primary identity includes all raw key bytes; secondary identity includes its own namespace ID,
  all secondary bytes and all primary bytes. Payer and deployment are retained provenance, not
  row identity. Table/index ID zero is valid. Treat emitted/ABI-declared IDs as authoritative.
- Binary ABIs require string table names, table/index IDs and complete trailing metadata.
  Decode secondary IDs through that deployment's secondary-index definitions. Reject missing or
  ambiguous required mappings explicitly; do not silently use a head ABI.
- Real contracts already need `slug_name` and enums. Rendered key strings can collide for
  noncanonical source values, so they cannot replace original bytes.
- Protobuf has an outer varuint32 and an inner size-varint framing layer, with node-specific JSON
  naming. Validate both bounds. A standalone protobuf payload decoder is insufficient.
- Profile selection applies to nested timestamps and both JavaScript/native or Rust paths.
  Changing a single block formatter or providing SHiP JSON alone is incomplete.

Pinned primary-source links and artifact limits live with the Wire package's
`docs/protocol-sources.md`; the [original investigation](../architecture/hyperion-4.5-wire-protocol.md)
remains a dated architecture record.

## Acceptance before calling Wire supported

Require complete codec conformance and unchanged default Antelope behavior first. Then exercise
the existing Node workers and required Rust consumers against the selected node/contracts/genesis
profile: live/bulk equivalent output, primary/secondary insert/update/delete, ABI deployment changes,
disconnect/reconnect, process restart and canonical fork replacement. Cover reversible-head default
and configured irreversible-only operation, durable progress, bounded recovery, failure isolation,
backpressure and affected-query gating under the applicable backend contracts.

Record exact source/component/build inputs, event/output counts, original keys, deployment/fork
identity, recovery/progress and every participating process. Publish stage/resource evidence before
choosing optimizations. Package preflight must reject missing/incompatible components before decoding;
upgrades remain operator-initiated and may require controlled restarts. This plan introduces no
new service, public native ABI, automatic updater or transport replacement requirement.
