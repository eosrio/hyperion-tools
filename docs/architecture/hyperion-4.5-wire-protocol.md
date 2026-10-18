# Hyperion 4.5: Wire protocol extension investigation

Status: Source investigation on 2026-10-17. The single operator-managed package boundary was
accepted under ADR 0030, and working Wire support is required for the 4.5 launch under ADR 0031.
Detailed implementation and qualification choices remain open.
No Wire extension has been implemented or qualified by this investigation.

## Inspected revisions

- `Wire-Network/wire-sysio`: `6508c9e8f4044df8d5acb082a7af3be6a80cd9a0`.
- `Wire-Network/hyperion-history-api`: `87b5e2e9647e0219fcf77d07b653668601e0ade7`.
- Its pinned `addons/node-abieos` submodule: `4e896b899e126441f0def767177c873ee6342918`.
- That addon's `abieos` submodule: `2ac0bdcbd74c8f0d7185c87089e401ff60deedd1`.
- Local `hyperion-tools` source inspected at HEAD `373c98f01c9c85f8c50c6917d1c47e63760e715d`.
  The architecture draft is untracked work and is not contained in that commit.

Public branch heads were resolved through the GitHub API. These revisions are investigation
baselines, not an assertion about software deployed on Wire's network or an accepted 4.5 support pin.

The published [v1.0.0 release](https://github.com/Wire-Network/wire-sysio/releases/tag/v1.0.0),
verified through the GitHub API on 2026-10-17, points to
`7dede884e0150d36fa788e85a119e10653ade8ca` and was published on 2026-07-30. Its usage qualification
in the release notes and the actual intended network build must be considered when choosing a
support target. Spot checks of that tag's ABI definition, timestamp configuration, and delta
producer found the table-ID/enum/protobuf metadata, 2025 epoch, and both KV streams described below.
This was not a complete release/master comparison or runtime qualification. The testnet's exact
node/contract pins remain open under D18a.

The accepted qualification target is a new Wire testnet to be launched with Wire's contracts;
see [ADR 0032](../adr/0032-qualify-wire-on-new-testnet-with-wire-contracts.md). At the time of that
decision the testnet did not yet exist. Its selected launch node, contract artifacts, and extension
versions will define the qualification profile. The public tag and investigation commits above
have not been accepted as those final pins.

## Node changes that the extension must accommodate

| Area | Verified node behavior | Consequence for the interface |
|---|---|---|
| Timestamp interpretation | Slots use a 2025 epoch with a 500 ms interval. | Select the chain's timestamp semantics before interpreting blocks, in live and bulk paths. |
| Binary contract ABI | Table names are strings; table definitions include a uint16 table ID and secondary-index metadata. | A legacy Antelope meta-ABI decoder cannot simply be assumed compatible. |
| ABI value codecs | ABI definitions include enums and optional protobuf descriptors, with corresponding serializer logic. | Protocol adaptation includes contract ABI/value codecs, beyond the SHiP envelope schema. |
| Primary KV deltas | `contract_row_kv` carries code, payer, table ID, raw key bytes, and raw value bytes. | Resolve table identity through the applicable ABI and preserve the complete source key. |
| Secondary KV deltas | `contract_index_kv` carries table ID and secondary/primary key bytes. | The common extension interface needs explicit handling/coverage for index records as well as row records. |
| Delta emission | The node emits both KV streams; its producer no longer requires the legacy row shape for these entries. | Do not infer protocol coverage from old comments about legacy `contract_row` translation. |

Primary sources:

- [Timestamp configuration](https://github.com/Wire-Network/wire-sysio/blob/6508c9e8f4044df8d5acb082a7af3be6a80cd9a0/libraries/chain/include/sysio/chain/config.hpp#L40-L42).
- [Contract ABI definitions](https://github.com/Wire-Network/wire-sysio/blob/6508c9e8f4044df8d5acb082a7af3be6a80cd9a0/libraries/chain/include/sysio/chain/abi_def.hpp#L60-L80)
  and [enum/protobuf fields](https://github.com/Wire-Network/wire-sysio/blob/6508c9e8f4044df8d5acb082a7af3be6a80cd9a0/libraries/chain/include/sysio/chain/abi_def.hpp#L106-L154).
- [Protobuf descriptor initialization and value decoding](https://github.com/Wire-Network/wire-sysio/blob/6508c9e8f4044df8d5acb082a7af3be6a80cd9a0/libraries/chain/abi_serializer.cpp#L283-L307).
- [SHiP primary/secondary KV schemas](https://github.com/Wire-Network/wire-sysio/blob/6508c9e8f4044df8d5acb082a7af3be6a80cd9a0/libraries/state_history/abi.cpp#L325-L340),
  [binary serialization](https://github.com/Wire-Network/wire-sysio/blob/6508c9e8f4044df8d5acb082a7af3be6a80cd9a0/libraries/state_history/include/sysio/state_history/serialization.hpp#L234-L261),
  and [delta emission](https://github.com/Wire-Network/wire-sysio/blob/6508c9e8f4044df8d5acb082a7af3be6a80cd9a0/libraries/state_history/create_deltas.cpp#L134-L137).

These findings do not establish that every historical Wire deployment used the same dialect or
that every listed node feature must be included in an as-yet-unagreed 4.5 Wire support baseline.

## Why a decoded contract hook is insufficient

Hyperion's contract handlers receive records after upstream decoding. A hook invoked at that point
cannot repair an incorrectly interpreted block timestamp, an incompatible binary contract ABI, or
an unsupported delta schema. Loading the SHiP handshake ABI supplies envelope/record descriptions;
it does not automatically configure every built-in scalar codec or contract meta-ABI implementation.

The accepted protocol/contract separation in ADR 0005 is therefore necessary. Its package/runtime
mechanism still needs definition. The JavaScript projection-hook runtime is not a complete
implementation plan for a Wire protocol extension.

## Existing local integration points

- `hyperion-history-api/src/indexer/workers/state-reader.ts`, `processFirstABI`, loads the handshake
  ABI into native and JavaScript paths before requesting data. Protocol selection must be available
  early enough to configure those paths.
- `hyperion-history-api/src/indexer/modules/loader.ts` loads parser modules and decoded action/delta
  handlers. Parser and pre-decode codec registration are different from downstream contract hooks.
- `hyperion-tools/crates/core/src/blocks.rs` currently hardcodes the 2000 epoch. Its block-log path
  needs the selected protocol's interpretation too; patching the Node decoder alone is insufficient.
- `hyperion-tools/Cargo.toml` selects `rs_abieos` 0.6 with its pure-Rust backend, and
  `crates/core/src/abi.rs` uses it for binary contract ABI decoding. A Node native addon does not
  automatically provide the equivalent Rust codec behavior. Current dependency support has not
  been comprehensively qualified for Wire here.
- `crates/core/src/ship.rs` loads the runtime handshake ABI but constructs request/ACK variants
  directly; scanning and offline tools also need explicit protocol coverage instead of assuming
  that an API-server plugin configures them.

## Independent investigation of the Hyperion fork

At the operator's request, a second agent independently traced the fork and its submodules. Its
source findings agree with the need for protocol-level extension interfaces and identify additional
qualification gaps:

| Finding | Source evidence and limits |
|---|---|
| ABI metadata paths differ | The fork's TypeScript meta-ABI includes the new table layout but ends at `variants`, omitting later node fields. Its JavaScript meta-ABI still describes legacy table names/layout. Omitting optional trailing data can lose metadata without necessarily producing an immediate parse failure. |
| Native binary-ABI source uses the old nested layout | The pinned C++ ABI structure uses `sysio::name` for table names and lacks table IDs/secondary-index definitions. Raw ABI loading decodes into this structure, and action-worker fallback paths call it. This mandatory layout mismatch is distinct from ignoring trailing fields. The addon imports a committed binary whose behavior/provenance was not tested here. |
| KV normalization is not lossless legacy identity | The fork retains decoded key components but selects only the first component, or the first after scope, as legacy `primary_key`. History document IDs use that selected value. Arbitrary compound keys sharing those components need a fuller identity contract. |
| Secondary deltas are intentionally ignored | `contract_index_kv` has an explicit no-op handler. Deriving backend indexes from primary rows may still support a particular projection; full source-secondary-stream coverage is not implemented by that handler. |
| Block-header assumption differs | The parser reads top-level `res.header_extensions`, while the node's result structures have no such field. Extensions belong to the block. This is a source mismatch; runtime failure behavior was not tested. |

Primary sources for the fork findings:

- [TypeScript meta-ABI](https://github.com/Wire-Network/hyperion-history-api/blob/87b5e2e9647e0219fcf77d07b653668601e0ade7/definitions/abi_def.ts#L82)
  and [JavaScript meta-ABI](https://github.com/Wire-Network/hyperion-history-api/blob/87b5e2e9647e0219fcf77d07b653668601e0ade7/addons/wirejs-native/wirejs-serialize.js#L1206).
- [Pinned native table layout](https://github.com/Wire-Network/abieos/blob/2ac0bdcbd74c8f0d7185c87089e401ff60deedd1/include/sysio/abi.hpp#L109),
  [binary ABI loading](https://github.com/Wire-Network/abieos/blob/2ac0bdcbd74c8f0d7185c87089e401ff60deedd1/src/abieos.cpp#L111),
  [worker fallback](https://github.com/Wire-Network/hyperion-history-api/blob/87b5e2e9647e0219fcf77d07b653668601e0ade7/workers/ds-pool.ts#L231),
  and [addon wrapper](https://github.com/Wire-Network/node-abieos/blob/4e896b899e126441f0def767177c873ee6342918/lib/abieos.ts#L1).
- [KV normalization and secondary handler](https://github.com/Wire-Network/hyperion-history-api/blob/87b5e2e9647e0219fcf77d07b653668601e0ade7/workers/deserializer.ts#L1573)
  and [history identity construction](https://github.com/Wire-Network/hyperion-history-api/blob/87b5e2e9647e0219fcf77d07b653668601e0ade7/helpers/elastic-routes.ts#L62).
- [Parser header handling](https://github.com/Wire-Network/hyperion-history-api/blob/87b5e2e9647e0219fcf77d07b653668601e0ade7/modules/parsers/4.0-parser.ts#L85)
  and [node result structures](https://github.com/Wire-Network/wire-sysio/blob/6508c9e8f4044df8d5acb082a7af3be6a80cd9a0/libraries/state_history/include/sysio/state_history/types.hpp#L122).

Some fallback paths use the head ABI when historical metadata is unavailable. That is evidence
about the fork, not an accepted fallback policy for 4.5: deployment-bound mapping/codec selection
and isolated recovery still apply. These observations do not prove failure for every existing
contract or any particular live Wire deployment.

## Extension package and implementation boundary

The operator accepted one chain extension package with separate internal components; see
[ADR 0030](../adr/0030-unified-chain-extension-package.md). Its installation, version matching, and
activation workflow must coordinate the applicable Node and Rust pieces for the operator.
The package can supply the following components, whose detailed contracts remain to be defined:

1. A chain/protocol profile and activation rules, including timestamp parameters and supported
   protocol/ABI dialects.
2. Codec providers for handshake/envelope, block/trace/delta, binary contract ABI, and value
   interpretation. Providers may use JavaScript, Rust, or native bindings as appropriate.
3. Normalization of logical tables and complete row/index identities into common events, retaining
   source bytes and protocol-specific metadata. A legacy scope/uint64 primary key is only one
   possible representation; it must not truncate a compound KV identity.
4. Optional contract definitions, projection hooks, and API integration that consume the normalized
   interface and continue to use the shared MongoDB/WormDB contracts.

Runtime initialization would register compatible providers before decoding. Live indexing and
historical/bulk tools would use the same declared protocol semantics. Native changes can require
rebuilding artifacts and restarting affected workers under the accepted reload policy; no decision
has required arbitrary native decoder replacement without restart.

The accepted boundary is a single plugin package with separate component interfaces. A common
JavaScript-only runtime for all extension roles is not implied by this choice. Exact
native ABI, static/dynamic linking, IPC/WASM choices, codec-provider language/build coverage,
package manifests, and process layout remain open; this document does not select them.

The contract projection-hook default was separately accepted in
[ADR 0033](../adr/0033-default-javascript-typescript-projection-hooks.md): JavaScript execution with
TypeScript authoring through the shared SDK. That choice does not prescribe the implementation
language of the protocol providers above.

## Qualification still required

Working Wire support is a mandatory 4.5 launch requirement under
[ADR 0031](../adr/0031-require-working-wire-support-for-45.md), including a MongoDB-only launch.
Applicable live ingestion and historical replay must work through the unified extension package.

Use fixtures from an explicitly pinned node/protocol baseline to compare applicable Node and Rust
paths. Cover binary ABI metadata and value codecs, timestamp interpretation, primary/secondary KV
records, compound keys, insert/update/delete, ABI changes, fork rollback, and live/bulk equivalence.
Preserve raw replay context and identify unsupported protocol features explicitly. Each downstream
state backend still needs its own conformance evidence.

This was a source inspection. No binaries were built or executed, no live Wire deployment was
queried, and no fixture suite was run. The existing Wire Hyperion fork is useful as an extraction
map for extension points; it is not by itself an end-to-end compatibility oracle.
