# Wire implementation delivery plan

Status: Active implementation frontier, 2026-10-21. This plan applies ADRs 0005 and 0030–0032;
it does not change the mandatory working-Wire release gate or select final testnet launch pins.
Preparation and qualification remain [tools #22](https://github.com/eosrio/hyperion-tools/issues/22),
coordinated through [release #24](https://github.com/eosrio/hyperion-tools/issues/24).

## Current implementation and evidence

### Production worker/MongoDB integration candidate — 2026-10-21

[History API #242](https://github.com/eosrio/hyperion-history-api/pull/242), code
`ad590df3fc820cbf9723c5d6530b5e88c4b967e4`, passes two complete native Linux Node
22/24 runs over original testnet blocks 1–1480 on Hyperion-3. Existing workers and
RabbitMQ feed the shared runtime and real MongoDB adapter; the real HTTP API returns
the independent physical-row/official-clio insert/update/delete oracle for two
`sysio.msgch` primary KV definitions. It binds the actual ABI deployment at block 222.

Actual Mongo update faults retain exact source intents and fence queries. Healthy
projections recover, a deliberately invalid mapping remains unavailable, and block
history/source acknowledgments continue. Durable admission before source decoding
and mapping prevents rejected diagnostic writes from reopening old state. Normal
unadmitted lag remains permitted. The new source/backend path adds no state barrier
to history acknowledgments and does not reuse the legacy Mongo delivery route.

The [checksummed source/run/observer evidence](https://github.com/eosrio/hyperion-history-api/tree/codex/portable-state-production/docs/performance/evidence/portable-state-worker-20261021)
retains exact versions, two held CRUD prefixes, actual ACKs, failure/recovery,
process exits and cleanup. Independent final Spec/Standards reviews have zero
actionable findings. Per-process CPU/RSS observations are limited to this paced,
fault-injected replay. Each run retains five one-copy Elasticsearch startup
acknowledgments; later reads do not strengthen those original guarantees.

The source uses upstream `6508c9e8`, Wire package `e8c7162` and SDK recovery contract
`414e7c2` from [SDK #13](https://github.com/eosrio/hyperion-sdk/pull/13). SDK #13 and
History API #242 remain pending merge. Test workers run inside an owned bounded
Linux container; test services/container are removed after evidence verification,
and the original Wire node, MongoDB, Elasticsearch and `tibs` services remain intact.

This is the M1 integration candidate for [#191](https://github.com/eosrio/hyperion-history-api/issues/191).
Full Wire support remains unqualified. [Wire #15](https://github.com/eosrio/hyperion-wire-plugin/issues/15)
records real `sysio::onblock` decoding discrepancies, including trailing bytes after
ABI deployment. The operator reports a separate rs_abieos compatibility implementation;
evaluate its PR against these original inputs rather than duplicate that work.
Required Rust consumers, contract/build/deployment receipts, secondary projections,
live/bulk equivalence and actual process-crash/fork recovery remain M3 gates. M2
auto-map/live mapping reload and stream-client integration #231 also remain required.

### Actual testnet input update — 2026-10-20

The operator's new testnet and synced SHiP node are available on Hyperion-3. The
[dated source record and original captures](../qualification/wire-testnet-20261020.md) bind the
observed node6508 binary, chain/genesis and schema to an offline Node 22/24 codec regression over
real block 1 input. The existing codec consumes all nine physical streams and 23 rows, including the
source-specific count discrepancy, without altering source bytes. Complete initialization and
canonical ABI coverage, contract/CDT/artifact pins, integrated Node/Rust paths and recovery
acceptance remain open. The earlier isolated `7dede884` engineering-chain evidence below is kept separate.

### Reviewed native-genesis candidates — 2026-10-20

[SDK PR #10](https://github.com/eosrio/hyperion-sdk/pull/10) adds the portable optional native-genesis
verification contract at `38dfdb08d3145ad7fdff1a004e9ae94a43c1fe83`.
[Wire PR #14](https://github.com/eosrio/hyperion-wire-plugin/pull/14) verifies the original 6508
initialization at `e8c7162437a0d0f4e2211979b959f2d002bf3552`.
[History API PR #241](https://github.com/eosrio/hyperion-history-api/pull/241), at
`2f2124e2bf6f24eab262c2283a33beb5f3341474`, re-verifies retained source with the selected provider,
then publishes durable ABI coverage and an issued current proof for the actual empty block-1
application frame. These three PRs are now merged; their original validation is a
bounded implementation checkpoint rather than release qualification.

The original absent trace section and null SHiP predecessor remain unchanged. The verified decoded
header's zero predecessor supplies checkpoint 0 → actual block 1 continuity. Native ABI states retain
physical ordinals and both present-empty barriers. Ordinary later blocks still require complete source
input. Reader range eligibility, null-parent light-block parsing and interrupted certificate publication
are covered by host regressions. The public action-coverage gate remains disabled.

A bounded read-only capture of actual blocks 2–4 from the same node/chain/schema extends the frozen
block-1 source. Original receipt SHA-256 is
`38d75b7a1ba28e3abd77512854f963e7723631d6837df73bd7aeff656cc8a450` (226,988 bytes); both unchanged
receipts are retained in the HAPI fixture. Each later header has zero transactions but one real trace
action. Native Node 22.23.3 and 24.19.0 pass the production parser/coordinator/proof/projection fixture
through actual block 4, including native ABI action bindings and skipped-block rejection. The full host
suite passes 881 tests. SDK and Wire local checks, packed consumers and independent Standards/Spec reviews
also pass. Original source capture involved no remote service/configuration writes.

This regression uses in-memory broker/storage seams. It does not complete
[production projection integration #191](https://github.com/eosrio/hyperion-history-api/issues/191),
required Rust genesis/consumer paths under [tools #41](https://github.com/eosrio/hyperion-tools/issues/41),
MongoDB/full-worker/API acceptance, contract build/deployment pins, or fork/reconnect qualification.
The 2026-10-21 candidate above supplies subsequent actual-source worker/MongoDB evidence
while retaining existing Elasticsearch history; merge and broader recovery/launch gates remain explicit.
The separate [stream-client gate #231](https://github.com/eosrio/hyperion-history-api/issues/231)
still requires filter, reconnect/recovery and cancellation compatibility.

### Earlier implementation frontier — 2026-10-19

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
artifact candidate. Its source manifest and checksums matched, and the isolated node now produces
blocks, exposes a captured SHiP ABI and passes a clean restart with chain/progress continuity.
The [setup runbook and receipts](../qualification/wire-node-setup.md) record the loopback endpoints,
bounded lifetime and exact evidence. This candidate is distinct from the newer source-derived
primitive profile and is not an accepted launch pin.

This first slice receives already-decoded fields. It does not implement SHiP frame decoding,
binary ABI/value codecs, ABI-deployment resolution, protocol loading, native distribution or live
fork recovery. A source bundle or passing fixture suite does not satisfy working Wire support.
Existing worker, retained-history and backend-neutral contracts remain unchanged.

The next Rust integration slice in [tools #41](https://github.com/eosrio/hyperion-tools/issues/41)
consumes the complete codec source in [Wire PR #5](https://github.com/eosrio/hyperion-wire-plugin/pull/5)
at an exact development revision. The [entrypoint support matrix and executable evidence](../qualification/wire-rust-provider.md)
separate provider-aware live ABI scanning and full raw frame replay from pending direct-disk,
bulk sink, snapshot and archive integration. Unsupported paths refuse Wire before outputs or
checkpoints change. This slice leaves #41 and the working-Wire release gate open.

## Executable frontier

| Delivery | Owner and issue | Verifiable result | Actual dependencies |
|---|---|---|---|
| Timestamp and KV primitives | [Wire #1](https://github.com/eosrio/hyperion-wire-plugin/issues/1) | Node/Rust agree on the 2025 epoch, slot bounds and all primary/secondary identity bytes, with complete payer/source/deployment provenance. Packed Node exports and internal Rust source share one profile and fixtures. | None. This implementation uses engineering fixtures and does not wait for testnet launch. |
| Complete Wire protocol/contract codecs | [Wire #2](https://github.com/eosrio/hyperion-wire-plugin/issues/2) | Pinned block/trace/ABI/KV bytes decode without losing metadata; enums, protobuf, slug values, nested timestamps, deployment changes and malformed inputs have executable oracles. | #1 supplies shared primitives. Source/build fixture preparation can proceed immediately. |
| Existing Node worker integration and additive SDK seam | [History API #212](https://github.com/eosrio/hyperion-history-api/issues/212) | A chain selects its provider before importing/constructing affected codecs; production reader/deserializer/output paths process the Wire fixtures while default Antelope behavior and retained history remain usable. | Complete #2 fixture for Wire acceptance. Generic default-provider extraction and SDK contract design can start in parallel. |
| Rust live/bulk/offline integration | [tools #41](https://github.com/eosrio/hyperion-tools/issues/41) | Applicable Rust paths select the same protocol semantics before fast-path decoding and produce equivalent output to Node. | Complete #2 fixtures. Independent of Node worker implementation; profile/fast-path extraction and Antelope regressions can start earlier. |
| Coordinated operator package and integrated qualification | [Wire #3](https://github.com/eosrio/hyperion-wire-plugin/issues/3) | One compatible internal component set installs/preflights/activates together, then passes selected-testnet live/replay and recovery exercises with retained evidence. | Node and Rust integration; selected launch inputs and provisioned testnet under tools #22. Inventory/build preparation can start earlier. |

This table preserves the workstream ownership and acceptance split. The original #1/#2
foundation work and the native-genesis host slices have progressed through merged PRs;
their partial evidence does not close coordinated Wire/testnet acceptance. Each row owns its implementation PRs in
the repository containing the change, with SDK changes linked from the integration issue.
Cross-repository dependencies are recorded natively where supported and in issue bodies.

The current next steps are merging the verified #191 integration candidate, evaluating the
separate rs_abieos compatibility PR against Wire #15, and completing required Rust and
testnet recovery qualification. The selected testnet is available. Existing profiling/recovery
issues retain their original acceptance requirements; this state integration does not close them.

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
| Testnet launch node/CDT builds | Actual running testnet reports node `6508c9e8f4044df8d5acb082a7af3be6a80cd9a0`; binary/version receipts are retained in the dated source record. Reproducible build/CDT inputs remain pending. |
| Deployed contracts and WASM/ABI artifacts | Pending inventory, build receipts and SHA-256 digests. A committed WASM beside C++ does not prove a matching build. |
| Genesis, chain identity/configuration and deployment sequence | Actual chain/genesis and selected public configuration are captured in the 2026-10-20 source record; deployment sequence and contract artifact pins remain pending. |
| Extension/SDK/host/Rust artifact versions and platform set | Pending tested coordinated set and selected packaging mechanism. No catalog support claim yet. |
| Isolated node setup | Dated tools #42 engineering setup on Hyperion-1; official release binary, RPC/SHIP and clean restart observed. The retained record is not a current uptime claim or the new testnet source. |
| Final testnet provisioning | Operator reports launched testnet; advancing node and real block1 SHiP input verified on Hyperion-3. Selected contract inventory, build/deployment receipts and full qualification remain pending. No production service replacement is part of this evidence slice. |

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
