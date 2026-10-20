# Hyperion 4.5 detailed decision record

Supporting reference for the [architecture definition](hyperion-4.5.md). Accepted decisions and
their rationale are preserved here. Open implementation questions are not additional release
requirements or assumptions that agents should treat as settled.

Status: Draft, being defined with the operator.

## Repository organization update — 2026-10-17

The operator approved the repository names and initial issue breakdown in the
[development roadmap](../roadmap/hyperion-4.5.md). The new SDK, AtomicAssets and Wire repositories
and reuse of the existing catalog are recorded in [ADR 0052](../adr/0052-repository-homes-and-release-coordination.md).
This resolves the repository-location questions for those components in older entries below;
backend-adapter source homes and detailed runtime module placement remain foundation work.
Approval of the plan does not mean the components have been implemented or qualified.

## Release and repository scope

Accepted framing from the operator:

- This architecture defines version 4.5 of `P:\eosrio\hyperion-history-api`.
- Most development will happen in `P:\eosrio\hyperion-tools`.
- Substantial changes to `hyperion-history-api` are part of the same release and architecture.
- Upgrading to 4.5 must not require a full history reindex. Keep existing Elasticsearch history
  usable and resume indexing, while initializing or rebuilding new or changed state projections
  separately. Qualify the supported upgrade baselines and their migration paths before release.
- The 4.5 upgrade includes plugin architecture improvements for dynamic contract-indexing definitions
  and support for chain protocol variations, using Wire Network's Hyperion fork as a concrete case.
- Full API parity for both AtomicAssets and AtomicMarket is a requirement for the 4.5 release,
  including if MongoDB is the only state backend available at launch.
- The parity baseline is `atomicassets/atomicassets-api` release `2.7.0`, pinned to commit
  `0aabc47dcf8bd0804d4dcecb336c90fc02a9c89e`.
- State APIs serve reversible head state by default, with correct fork rollback. Operators may
  select irreversible-only state serving per chain; either backend must satisfy the selected mode.
- The shared runtime coordinates state-projection fork recovery and query gating. Backend adapters
  implement rollback of stored projection effects; plugins keep producing backend-neutral mutations.
- State backend adapters are independently versioned extensions. Hyperion releases pin qualified
  adapter versions so operators receive a tested combination without manual compatibility matching.
- Backend cutover is operator-initiated and may briefly pause the affected chain's state API. The
  destination must be initialized, caught up, and validated before cutover; existing history stays usable.
- RabbitMQ remains the default ingestion transport for 4.5. An explicit transport interface is
  follow-up work; its extraction is not an additional launch requirement from this decision.
- One first-party AtomicAssets plugin includes AtomicAssets and AtomicMarket indexing and APIs,
  maintained by EOS Rio and hosted through Hyperion's indexing workers and existing Fastify API
  server. It has one operator-facing package, version, and release with modular internals.
- First-party plugins release independently. Each Hyperion release pins tested compatible plugin
  versions so operators receive a supported combination without resolving compatibility manually.
- The shared plugin SDK versions independently of core and plugins. Preserve backward compatibility
  within each SDK major; breaking interface or semantic changes require a new SDK major.
- Executable plugin upgrades are operator-initiated. EOS Rio provides an official plugin catalog
  repository where compatibility is declared; catalog changes do not automatically replace running
  plugin code. ABI-driven mapping updates retain their accepted lifecycle within allowed contracts.
- Standard 4.5 distributions include those pinned first-party plugins, disabled by default and
  enabled per chain through configuration.
- Live reload of declarative contract mappings is mandatory for the 4.5 launch. Operators must be
  able to add or update mappings without restarting the indexer, with validation and staged
  rebuild/activation where required. Hooks and native protocol components may use controlled restarts.
- Contract-indexing plugins must declare each projection's supported initialization strategies and
  required source coverage. Hyperion verifies that the available sources satisfy those requirements
  before marking the projection ready; snapshot catch-up and historical backfill are distinct needs.
- The shared Hyperion runtime owns initialization/backfill/rebuild job scheduling, durable progress,
  retry/resume, and readiness publication. Plugins declare requirements and provide domain logic;
  evolve existing worker/tool infrastructure to execute those jobs.
- Use a configurable bounded local recovery window for isolated projection failures, with historical
  sources for older inputs. If required data is unavailable, mark the projection incomplete and keep
  unrelated indexing running; do not silently skip missing inputs or claim completed progress.
- Indexing uses at-least-once delivery with idempotent processing. Consumers acknowledge completed
  work only after its effects are durably stored and restart progress is recoverable.
- Keep existing queue backpressure for infrastructure outages: pause the shared reader when an
  Elasticsearch queue reaches its budget and resume after it drains to the resume threshold.
  Contract-specific failures remain isolated and must not halt chain indexing.
- Declarative mappings and projection hooks produce backend-neutral mutation batches. Core/runtime
  owns their validated application and recovery lifecycle through the selected backend adapter.
- Target deterministic, replay-safe projection hooks where feasible. Qualify the shared SDK and
  hook behavior against versioned inputs and declared prior state; exact enforcement and the scope
  of the guarantee remain open. Existing replay/idempotency correctness requirements still apply.
- The 4.5 default for executable contract projection hooks is JavaScript in Hyperion's Node.js
  runtime, with TypeScript authoring through the shared SDK. Protocol codecs and backend
  implementations keep their separate interfaces and implementation-language choices.
- Executable plugins are trusted, operator-installed code in 4.5. Operators are responsible for
  auditing the plugins they choose to use. Compatibility checks, shared interface contracts, and
  failure isolation still apply; a sandbox for untrusted plugins is follow-up work.
- Evolve Hyperion's existing master/worker infrastructure as the execution baseline. Build on its
  live/history worker roles, contract-usage assignments, relaunch, and queue scaling/backpressure;
  qualify targeted changes for the new projection contracts before selecting a new pool layout.
- Live indexing has priority by default when live and historical work compete for resources.
  Protect live-processing capacity and throttle bulk work first under contention; historical work
  can use spare capacity, while bulk-only profiles can use their full configured budget.
- Preserve event order per projection by default; permit parallel execution across explicitly
  declared independent partitions. Live reading may process independent contracts from the same
  block concurrently. Historical bulk filling may use advanced strategies with equivalent results.
- Definitions/plugins explicitly declare atomic groups when multiple records must commit together.
  Validate this backend capability before activation; ordinary batches retain ordered, idempotent,
  recoverable application without automatically requiring a multi-record transaction.
- Plugins express reads through a shared structured logical query model. Native database query
  construction stays inside backend adapters so the same plugin queries work with MongoDB and
  WormDB; adapters translate and optimize execution while preserving the declared semantics.
- Snapshot consistency is declared per endpoint. Plugins require a shared read snapshot where
  related queries need one stable view; ordinary reads use the latest eligible state. Broader
  consistency defaults may be improved later.
- When a failed required projection prevents an endpoint from answering correctly, return a
  retryable unavailable error by default. A plugin may explicitly support serving last-valid state;
  unrelated queries and indexing continue.
- If a plugin serves last-valid state after a projection failure, the response must include the
  block number through which that served view is complete. Bind the marker to the actual data,
  including cached responses; row modification blocks and current chain head are different values.
- HTTP plugins use shared Hyperion headers for that block number and a last-valid-state indicator,
  preserving compatible JSON response bodies. Header names and additional metadata remain to be
  specified through the shared API contract.
- Enabling a plugin automatically resolves and activates its required indexing dependencies within
  the operator's configured contract scope. Dependency activation does not automatically expose
  another plugin's public APIs; core validates compatibility and prerequisite readiness.
- Chain extensions are one operator-managed package with separate internal components. Package
  tooling coordinates the required Node/Rust codec providers and their compatibility; operators
  do not install, version-match, or activate those pieces separately.
- Working Wire protocol-extension support is required for the 4.5 launch. Qualify applicable live
  ingestion and historical replay through the unified package against an agreed, pinned Wire
  node/protocol baseline. This requirement also applies to a MongoDB-only launch.
- The Wire acceptance target is a new testnet to be launched with Wire's contracts. The node,
  contracts, and extension versions selected for that launch will define the qualification profile;
  their exact revisions and deployed artifacts remain to be pinned.
- This document is maintained in `hyperion-tools` during drafting and covers the participating
  repositories together. Ownership of individual modules and interfaces will be resolved explicitly.

This document will define the architecture that implementation agents follow across Hyperion Tools,
Hyperion History API, WormDB, the composed WormDB server, and its serving domains. It describes the
intended system, its contracts, and the reasons for its decisions. Existing implementation is evidence
of available capabilities; it does not determine the target architecture by itself.

## How this document becomes authoritative

- Record a decision as accepted only after the operator agrees to it during the architecture discussion.
- Keep proposed answers and unresolved questions explicit. Agents must not implement them as settled decisions.
- Once accepted, a decision applies across the participating repositories. Conflicts with older plans
  must be identified and reconciled rather than silently resolved by an implementation agent.
- Separate target architecture from delivery sequence and from implementation/validation status.
- Record meaningful changes to accepted decisions with their rationale and affected contracts.

Accepted decisions are marked in the sections below. Other architecture and delivery decisions remain
open. The existing requirements and vision remain inputs to reconcile; this draft records where the
operator has now clarified their intent.

## Chain indexing invariant

Contract-specific issues must never halt chain indexing. Operators do not control the smart
contracts whose activity Hyperion observes. Missing or incompatible contract mappings, contract ABI
problems, and failures in contract-specific processing must be isolated from shared source capture
and unrelated indexing. D13h defines the contract-failure policy.

## Existing MongoDB implementation is the baseline

The operator clarified that much of the contract-state functionality discussed here already exists
in Hyperion for MongoDB. Source inspection confirms configured contract/table selection, ABI-derived
physical indexes, manual index overrides, contract configuration reload through the controller and
deserializer workers, live row upserts/deletions, state synchronization, and a state table-query API.
Historical ABI storage and block-based ABI lookup also exist; they must not be confused with the
separate current-ABI lookup used to create Mongo indexes.

Use these mechanisms as the starting point for 4.5. Describing an accepted target does not mean its
entire implementation is new. Implementation plans must distinguish existing behavior to retain,
behavior to extend or consolidate, and capabilities still to implement or qualify for WormDB.
Backend-independent definitions, mapping revision/activation guarantees, and account-scoped full
auto-map are extensions to reconcile with the existing code.

The [MongoDB implementation baseline](hyperion-4.5-existing-mongodb.md) records the inspected paths
and their limits. This is source-level evidence, not an end-to-end acceptance result. The existing
Mongo query implementation also does not settle the API compatibility contract still open in D02/D07.

## Starting intent from the existing requirements

Serve Hyperion v2, Light API, AtomicAssets/AtomicMarket, and native chain v1 through shared operator
infrastructure. Reduce independently maintained ingestion and serving stacks while preserving the
client behavior required to replace them.

This intent comes from `benchmark/atomicassets/REQUIREMENTS.md` and the vision page. Its precise scope,
constraints, and acceptance conditions will be recorded through the decisions below.

## Decisions to resolve

The order is provisional; dependencies discovered in discussion may change it.

| ID | Decision | Status |
|---|---|---|
| D01 | The role of WormDB and MongoDB in the finished state tier | Accepted — permanent alternatives |
| D01a | Granularity at which an operator selects a state backend | Accepted — per chain; supersedes per-domain selection |
| D01b | Backend adapter packaging and release policy | Accepted — independently versioned extensions; qualified adapter versions pinned by Hyperion |
| D01c | Backend change workflow and cutover availability | Accepted — caught-up, validated destination before operator cutover; brief state-API pause allowed |
| D02 | Supported API families, versions, chains, and compatibility obligations | Full AtomicAssets/AtomicMarket parity required for 4.5; API baseline pinned; detailed contract and chain/contract coverage open |
| D02a | AtomicAssets/AtomicMarket parity reference | Accepted — API 2.7.0 at exact commit 0aabc47dcf8bd0804d4dcecb336c90fc02a9c89e |
| D03 | Ownership of authoritative facts and derived serving projections | Projection rule accepted; detailed contracts open |
| D04 | Shared ingestion, source transport, decoding, and sink independence | Accepted — RabbitMQ default for 4.5; transport interface follow-up; detailed ingestion contracts open |
| D05 | Finality, reversible state, block identity, and replay semantics | Accepted — reversible head by default, correct fork rollback, optional irreversible-only mode per chain; detailed contracts open |
| D05a | Ownership of state-projection fork recovery | Accepted — runtime coordinates recovery; adapters implement rollback; plugins produce backend-neutral mutations |
| D06 | Serving views, cross-tier visibility, and degraded behavior | Fork-recovery and isolated-projection-failure query policies accepted; detailed visibility contracts open |
| D06a | Consistency of responses combining state and history | Independent state reads accepted; common-block proposal withdrawn; endpoint-specific contracts open |
| D06b | Serving during isolated projection failures | Accepted — retryable unavailable by default; explicit plugin support for last-valid state with mandatory served-view block number |
| D06c | Transport for last-valid-state response metadata | Accepted — shared Hyperion HTTP headers across plugins; compatible JSON bodies preserved |
| D07 | Shared query interface, planning, historical filter/sort keys, pagination, and counts | Shared query model accepted — native queries confined to adapters; detailed semantics open |
| D07a | Consistency across related reads within one API request | Accepted — snapshot consistency declared per endpoint; broader defaults may evolve later |
| D07b | Request and cache binding during backend/projection cutover | Accepted — bind requests and cached responses to compatible backend/projection versions |
| D08 | Segment layout, compaction, generation publication, and retention | Open |
| D09 | Durability, restart recovery, and source coverage | Accepted — at-least-once delivery, idempotent processing, acknowledgment after durable completion; detailed recovery/source contracts open |
| D10 | Process layout, gateway execution, and module/repository ownership | Accepted — combined EOS Rio AtomicAssets plugin in existing API server; detailed ownership/process contracts open |
| D10a | Official plugin release/version policy | Accepted — independent releases; tested compatible versions pinned for each Hyperion release |
| D10b | First-party plugin distribution and activation defaults | Accepted — pinned plugins included, disabled by default, enabled per chain through configuration |
| D10c | AtomicAssets plugin packaging | Accepted — one plugin includes AtomicAssets and AtomicMarket indexing and APIs, with one version/release |
| D10d | Executable plugin upgrades and official catalog | Accepted — operator-initiated upgrades; official repository catalog declares compatibility |
| D11 | Deployment profiles, resource budgets, and operational interfaces | Open |
| D11a | Backpressure during infrastructure outages or shared capacity exhaustion | Accepted — retain shared-reader pause/resume; contract failures remain isolated |
| D11b | Resource priority when live and historical work coexist | Accepted — live indexing takes priority by default; budgets and scheduling mechanisms open |
| D12 | Conformance tests, acceptance evidence, and delivery sequence | Open |
| D12a | Backend availability in the 4.5 release | Accepted — MongoDB-only launch permitted if WormDB effort is too high |
| D13 | Dynamic contract indexing and protocol extension interfaces | Accepted — separate protocol and contract interfaces |
| D13a | Authoring model for dynamic contract-indexing definitions | Accepted — declarative first, portable across backends, code hooks |
| D13b | Backend capability checks when activating a definition | Accepted — validate requirements before activation |
| D13c | Loading and activation lifecycle for plugins/definitions | Accepted direction — reload declarative mappings live; controlled restarts allowed for hooks/native protocol components |
| D13d | Scope of live configuration reload in 4.5 | Accepted — declarative contract mapping reload is mandatory for launch; detailed activation contract open |
| D13e | Serving behavior while changed mappings are rebuilt | Accepted — previous version serves during staged rebuild |
| D13f | Contract mapping versions and ABI deployments | Accepted — mappings are versioned by ABI deployment |
| D13g | Mapping revisions within one ABI deployment | Accepted — independent immutable revisions |
| D13h | Contract-specific failure isolation | Accepted — contract issues never halt chain indexing |
| D13i | Automatic mapping generation and ABI evolution | Accepted — full auto-map mode for allowed accounts |
| D13j | Automatic physical index creation policy | Accepted direction — automatic selection with simple operator configuration |
| D14 | Plugin/backend execution contract and projection-write ownership | Accepted — managed mutation batches for mappings and projection hooks |
| D14a | Projection event ordering, hook reads, and parallelism | Accepted — ordered projections by default, declared independent partitions, distinct live/bulk execution strategies |
| D14b | Mutation-batch atomicity | Accepted — explicitly declared atomic groups as backend capabilities |
| D14c | Replay determinism of projection hooks | Direction accepted — pursue deterministic replay-safe hooks where feasible; enforcement and release obligations open |
| D15 | Plugin dependencies and activation | Accepted — resolve required indexing dependencies automatically, with API domains enabled separately |
| D16 | Contract projection hook runtime | Accepted — JavaScript execution with TypeScript authoring through the shared SDK; isolation/SDK details open |
| D16a | Worker infrastructure evolution and hook isolation | Direction accepted — evolve existing infrastructure; exact hook-pool/process placement remains open |
| D16b | Executable plugin trust model for 4.5 | Accepted — trusted operator-installed code; operators audit their chosen plugins; untrusted-code sandboxing deferred |
| D16c | Shared plugin SDK compatibility/versioning | Accepted — independently versioned SDK; backward compatibility within each major; new major for breaking changes |
| D17 | Chain protocol extension package and runtime providers | Accepted — one operator package coordinates separate internal Node/Rust components; implementation mechanisms open |
| D18 | Wire as a 4.5 protocol-extension acceptance case | Accepted — working Wire support required for launch; exact node/protocol baseline and qualification matrix open |
| D18a | Required Wire network/node compatibility baseline | Target accepted — a new Wire testnet with Wire contracts; exact node/contract/extension pins open |
| D19 | Upgrade compatibility for existing Hyperion deployments | Accepted — preserve usable Elasticsearch history without requiring a full history reindex; state projections initialize/rebuild separately |
| D20 | Projection initialization and required source coverage | Accepted — mandatory per-projection initialization declarations and source-coverage validation before readiness |
| D20a | Initialization and rebuild job ownership | Accepted — shared Hyperion orchestration; plugin-defined requirements and domain logic |
| D20b | Recovery source retention and unavailable ranges | Accepted — configurable bounded recovery window, historical sources for older inputs, incomplete projections when data is unavailable |

## D01 — State backend roles

Status: Accepted.

The target architecture retains MongoDB and WormDB as permanently supported alternatives for state
queries. Neither is designated as a temporary backend to be retired by this architecture. Agents must
preserve this target when developing the state-query architecture. Backend availability in the first
4.5 release is qualified separately under D12a; both backends need not ship simultaneously.

The shared client-visible behavior, supported query surface, and backend conformance obligations will
be defined explicitly in D02/D07/D12. Backend selection granularity is defined in D01a. Migration,
running both for comparison, and failure behavior remain separate decisions.

### WormDB project context

- WormDB is a database the operator is building from scratch and continues to update.
- The operator reports that WormDB is being proven in production while development continues.
- Local engine repository: `P:\wormdb`.
- Public source: [igorls/wormdb](https://github.com/igorls/wormdb), verified accessible on 2026-10-15.
- The earlier plan was written approximately four months before this discussion, when WormDB was not
  yet public. Its availability assumptions must be updated when reconciling the older documents.

Rationale recorded in [ADR 0001](../adr/0001-permanent-state-backend-alternatives.md).

## D01a — Backend selection granularity

Status: Accepted.

Operators select one state backend for each chain. All enabled state-query domains for that chain use
the selected backend. Different chains may select different backends.

The earlier decision to allow independent selection for each `(chain, API domain)` pair is superseded.
The operator changed this decision to avoid the complexity and edge cases of overlapping state
ownership and cross-backend queries within one chain.

This decision concerns the database choice. Physical segmentation, process layout, backend migration,
and temporary parallel validation remain separate design decisions.

Current rationale: [ADR 0003](../adr/0003-select-state-backend-per-chain.md).
Superseded decision: [ADR 0002](../adr/0002-select-state-backend-per-chain-and-domain.md).

## D01b — Backend adapter packaging and release policy

Status: Accepted on 2026-10-17. Exact packaging, SDK boundaries, and repository ownership remain open.

State backend adapters use a dedicated extension interface and release independently,
with qualified adapter versions pinned by each Hyperion release. Operators continue to select one
backend per chain and receive a tested combination through the distribution/tooling; they should
not need to resolve adapter/core version compatibility manually.

Database engine versions remain separate from adapter package versions. Exact packaging, SDK
boundaries, catalog metadata, and repository ownership remain open. D12a's MongoDB-only launch
allowance still applies; this packaging policy does not require an unqualified WormDB adapter to ship.

Rationale recorded in [ADR 0049](../adr/0049-independently-versioned-backend-adapter-extensions.md).

## D01c — Backend changes and cutover availability

Status: Cutover policy accepted on 2026-10-17. Detailed migration mechanics and tooling remain open.

Backend changes are operator-initiated. Initialize, catch up, and validate the destination state
projections before an explicit cutover. A brief pause of the affected chain's state API is acceptable
for 4.5, provided the destination is caught up and validated before switching.
Keep existing Elasticsearch history usable under D19.

A staged destination is not a second active serving backend: all enabled state domains still use
one backend per chain under D01a. Exact write routing, source/catch-up coverage, checkpoints,
validation, rollback, and tooling remain open. This lifecycle policy does not establish an implemented
migration tool, add a zero-downtime cutover requirement, or require both adapters at the initial
4.5 launch.

Rationale recorded in [ADR 0050](../adr/0050-validated-backend-cutover-with-brief-state-api-pause.md).

## D02 — API coverage and release obligations

Status: Full AtomicAssets and AtomicMarket parity accepted as a 4.5 release requirement on 2026-10-16.
The reference API version/commit is accepted under D02a. Chain/contract deployment coverage and
detailed compatibility/acceptance contracts remain open.

The older `benchmark/atomicassets/REQUIREMENTS.md` targets full AtomicAssets API parity for existing
Hyperion operators, including AtomicMarket-related serving work. Its rationale is that missing
endpoints force operators to retain their separate stack, preventing the intended consolidation.

Hyperion 4.5 must deliver full API parity for both AtomicAssets and AtomicMarket. An implementation
with partial endpoint/behavior coverage does not satisfy this release requirement. D12a permits
a MongoDB-only launch if WormDB qualification requires too much effort; it does not reduce the
required API coverage. Any backend offered as supported for this surface must satisfy the same
client-visible compatibility contract.

Parity concerns client-visible behavior, not merely the existence of route names. Use the API
reference pinned in D02a, select supported chain/contract deployments, and enumerate the required
endpoints, request/response semantics, filtering, ordering, pagination, counts, history/derived
values, and other supported interfaces in the conformance contract. The supported chain/contract
deployment scope and acceptance evidence remain to be agreed. The accepted API pin must not
silently become a moving release target.

### D02a — Pinned parity reference

Status: Accepted on 2026-10-16.

| Item | Accepted value |
|---|---|
| Reference repository | [atomicassets/atomicassets-api](https://github.com/atomicassets/atomicassets-api) |
| Release | [2.7.0](https://github.com/atomicassets/atomicassets-api/releases/tag/2.7.0) |
| Exact commit | [0aabc47dcf8bd0804d4dcecb336c90fc02a9c89e](https://github.com/atomicassets/atomicassets-api/commit/0aabc47dcf8bd0804d4dcecb336c90fc02a9c89e) |
| Parity scope governed by this pin | AtomicAssets and AtomicMarket client-visible behavior |

Implementation and conformance work must use this immutable commit as the reference. A local
checkout or a moving upstream branch/tag is insufficient on its own. Later upstream releases do
not automatically change the 4.5 acceptance target. Changes to this baseline must be recorded as
explicit architecture changes and the affected compatibility obligations requalified.

Rationale recorded in [ADR 0014](../adr/0014-pin-atomic-api-270-parity-baseline.md).

### Reference-version evidence and older compatibility research

Checked upstream on 2026-10-16 through GitHub's releases API. The latest non-prerelease of
`atomicassets/atomicassets-api` is [2.7.0](https://github.com/atomicassets/atomicassets-api/releases/tag/2.7.0),
published on 2026-10-13, resolving to commit `0aabc47dcf8bd0804d4dcecb336c90fc02a9c89e`.
The local reference checkout reports package version 1.7.17 at commit
`63fbf1bdf35ee3cc8818314ce1954070712841d4`; it is not evidence of the current upstream release.

The older compatibility matrix records June's v2 release-candidate research. The released
[AtomicAssets contract v2.0.0](https://github.com/atomicassets/atomicassets-contract/releases/tag/v2.0.0)
differs from that research: custodial rentals were removed from the release. In particular, its
older holder/move requirements must be rechecked against the selected deployment and reference
version before being treated as implementation requirements. Historical deployments remain a
separate compatibility concern.

Contract deployment/version coverage and the complete conformance inventory remain open. API
version 2.7.0 does not by itself specify every chain deployment to support.

Rationale recorded in [ADR 0013](../adr/0013-require-atomicassets-atomicmarket-parity-for-45.md).

## D03 — Authoritative facts and serving projections

Status: Projection rule accepted; detailed ownership and recovery contracts remain open.

The older requirements use an absolute "each datum once" rule. Per-chain backend selection removes
the proposed mixed-backend ownership problem. The accepted materialization rule below defines how
historical values may also participate in state queries.

For example, exact filtering and sorting by `transferred_at` or true `template_mint` may require the
state backend to index values derived from historical events. Hydrating those values after pagination
cannot fix a page whose membership or ordering was selected using different values.

Permit explicitly owned, rebuildable query keys and projections in the chain's selected state backend,
including history-derived values required for exact state queries.
Keep full lifecycle history in the history/archive tiers. Define authority, provenance, and rebuilding
instead of prohibiting every repeated field. The exact ownership, consistency, and retention contracts
remain to be specified.

Rationale recorded in [ADR 0004](../adr/0004-materialize-history-derived-state-query-fields.md).

### Plugin-specific example: AtomicAssets burned assets

Burned-asset semantics belong to the AtomicAssets plugin under the accepted separation in
D10/D13a. Its definitions/hooks and query code own burn handling, the required serving records,
field/query semantics, and historical backfill. Core provides generic event, projection, storage,
indexing, capability, replay, and rollback contracts; backend adapters implement those contracts.
Adding this domain behavior must follow the existing requirement to avoid contract-specific core
changes and preserve portable definitions across MongoDB/WormDB.

The pinned [asset filters](https://github.com/atomicassets/atomicassets-api/blob/0aabc47dcf8bd0804d4dcecb336c90fc02a9c89e/src/api/namespaces/atomicassets/utils.ts)
support burned-asset queries. Current snapshots contain live chain rows, while the existing snapshot
loader omits already-burned records and the experimental WormDB path drops burned tombstones during
compaction (`crates/snapshot-load/src/atomicassets.rs`, `crates/wseg-build/src/aa_live.rs`). These are
AA plugin implementation/parity considerations, not a separate core architecture decision.

No universal retention policy for burned/deleted entities is established here. The AA plugin's
projection/query design must satisfy full parity and D03's ownership/rebuildability rules. Generic
auto-map row deletion must not acquire AtomicAssets-specific behavior. The shared logical operation
set and backend capability contracts remain the core design questions to resolve.

## D04 — Shared ingestion and transport

Status: RabbitMQ default accepted on 2026-10-16. Transport-interface extraction is follow-up work;
detailed ingestion contracts remain open.

The existing requirements call for one shared SHiP ingestion path feeding the serving tiers.
Hyperion already uses RabbitMQ between the reader, deserialization, and indexing workers. The
reader publishes block messages with persistence requested; deserializers route processed data
into separate state/history queues, and the master provisions their ingestor workers separately.
These are existing implementation mechanisms, not a new 4.5 queue system. This inspection does
not qualify end-to-end durability or replay guarantees for every path.

Source entry points in `hyperion-history-api`: `src/indexer/workers/state-reader.ts`,
`src/indexer/workers/deserializer.ts`, and `src/indexer/modules/master.ts` (`setupIndexers`).

Retain RabbitMQ as the default ingestion transport for Hyperion 4.5. Build on the existing queue
infrastructure when integrating state/history consumers, contract indexing, and the selected
state backend. State and history progress independently under D06a; contract-specific failures
follow D13h.

An explicit transport interface is follow-up work. Track its design/extraction without making it
an additional 4.5 launch gate or assuming that alternate transports ship in 4.5. Replacing RabbitMQ
is not a requirement of this release.

Shared ingestion describes a coordinated chain feed; it does not require a single process or
connection or prohibit parallel historical-range workers. The event format, delivery/acknowledgment
semantics, source retention, restart/replay guarantees, and module/process ownership remain separate
decisions under D04/D09/D10.

Rationale recorded in [ADR 0017](../adr/0017-retain-rabbitmq-default-for-45.md).

## D05 — Finality and reversible state

Status: Serving policy accepted on 2026-10-16. Detailed visibility, block identity, and
recovery/checkpoint contracts remain open.

State APIs serve the latest successfully processed canonical reversible head state by default.
This is an indexed view of the chain; it does not imply zero processing lag or direct access to
the instantaneous chain tip. Correct rollback/replay after forks is required.

Operators may select irreversible-only state serving per chain. In that mode, the visible state
must correspond to a fully applied canonical position at or below the last irreversible block
(LIB). Reversible changes must not leak into that view. Filtering current rows by their last-update
block is not sufficient to reconstruct earlier ownership, deleted rows, or prior field values.

All enabled state-query domains on the chain follow its selected mode. Both MongoDB and WormDB
must satisfy the selected mode's capabilities and correctness requirements. A backend offered for
default reversible serving requires qualified fork rollback; the MongoDB-only release allowance
does not weaken this requirement.

Fork recovery must undo or exclude orphaned-branch effects and apply the replacement canonical
branch to affected projections and their query indexes/derived values. ABI-deployment and mapping
selection must follow the applicable branch. The physical undo, versioning, or replay mechanism
remains to be designed; this decision does not prescribe a backend's storage implementation.

### Existing implementation evidence

Current-source evidence:

- Hyperion's `src/indexer/workers/state-reader.ts` requests SHiP data with
  `irreversible_only: false` and contains fork detection/cleanup paths.
- The [reference reader configuration at the pinned API commit](https://github.com/atomicassets/atomicassets-api/blob/0aabc47dcf8bd0804d4dcecb336c90fc02a9c89e/config/readers.config.example.json)
  also sets `irreversible_only: false` and has a separate minimum-confirmation setting. Reading
  reversible blocks does not imply a guarantee to expose the instantaneous chain tip.

These facts establish existing reversible-input support. This inspection has not qualified complete
state rollback behavior for MongoDB or WormDB.

Queries pause during affected fork recovery under D06. Detailed in-flight/cached-response handling,
confirmation-depth policy, cross-tier consistency, finality-mode changes, and recovery/checkpoint
contracts remain open.

Rationale recorded in [ADR 0015](../adr/0015-reversible-head-with-optional-irreversible-state.md).

## D05a — Ownership of state-projection fork recovery

Status: Accepted on 2026-10-17. Exact rollback/checkpoint and crash-recovery interfaces remain open.

The shared runtime owns canonical-branch/fork coordination, identifies affected
projections, and manages the query pause, rollback, and replacement-branch application lifecycle.
Backend adapters implement the backend-specific reversible-state operations behind a common
contract. Projection hooks continue to produce declared logical mutations; fork recovery of those
stored effects belongs to the shared runtime/backend contract.

This builds on D14's runtime-managed writes without prescribing an undo-log, multi-version,
before-image, or other physical implementation for either backend. Protocol adapters retain their
chain interpretation role. External notifications and other side effects have separate recovery
requirements. Exact block/checkpoint identities, operation signatures, publication barriers, and
crash recovery during a fork remain open.

Rationale recorded in [ADR 0048](../adr/0048-runtime-coordinates-forks-and-adapters-rollback-state.md).

## D06 — Query visibility during fork recovery

Status: Fork-recovery query policy accepted on 2026-10-16. Detailed error and visibility contracts
remain open.

D05 requires correct fork rollback. Pause affected queries during fork recovery until a consistent
canonical view is ready. Once a branch is known to be orphaned, affected queries must not continue
serving its previous published view while awaiting replacement.

Requests may wait within their request timeout; if recovery outlasts that timeout,
return a retryable unavailable response using the API's supported error contract. Do not expose
intermediate undo/replay mutations. Unaffected queries and other chains may continue serving.

The accepted availability tradeoff is a brief pause during recovery. Precise HTTP/error behavior,
in-flight requests, caches, subscriptions, and cross-tier visibility remain to be specified.

Rationale recorded in [ADR 0016](../adr/0016-pause-affected-queries-during-fork-recovery.md).

## D06a — Responses that combine state and history

Status: Independent state reads accepted on 2026-10-16; endpoint-specific contracts remain open.
The blanket common-block proposal was withdrawn and was never accepted.

State reads advance independently of history indexing and do not wait for history's progress.
Additional coordination must be justified by an endpoint's actual reference semantics. The
architecture does not require a universal same-block publication/read barrier between the tiers.

The operator expects state indexing to update faster than history under normal operation. This is
the expected operating model for discussion, not a measured latency claim or an invariant that
every state write always becomes visible before the corresponding historical record. The inspected
Hyperion deserializer queues dynamic-state updates before its history-delta path, while independent
state/history ingestor queues and workers complete the storage work separately. Enqueue order does
not establish cross-store write-completion order.

The requirements route state to MongoDB/WormDB and history to Elasticsearch/archive. A concrete
single-response example is AtomicAssets `/v1/transfers`:

- The pinned [transfer route](https://github.com/atomicassets/atomicassets-api/blob/0aabc47dcf8bd0804d4dcecb336c90fc02a9c89e/src/api/namespaces/atomicassets/routes/transfers.ts)
  reads transfer records and then calls `fillTransfers` to return embedded asset objects.
- The pinned [filler](https://github.com/atomicassets/atomicassets-api/blob/0aabc47dcf8bd0804d4dcecb336c90fc02a9c89e/src/api/namespaces/atomicassets/filler.ts)
  hydrates those asset IDs through an asset view. This does not reconstruct each asset at its
  historical transfer time. If a lookup is missing, `AssetFiller.fill` falls back to the asset ID
  string; a missing embedded object alone therefore does not establish a parity violation.
- The pinned [transfer handler](https://github.com/atomicassets/atomicassets-api/blob/0aabc47dcf8bd0804d4dcecb336c90fc02a9c89e/src/api/namespaces/atomicassets/handlers/transfers.ts)
  also joins transfer/asset records to apply collection/template/schema filters before pagination
  and count selection.

In the planned split, a transfer history query may therefore require state-side asset lookups for
filtering or response enrichment. With state ahead of history, the normal result is available
historical transfers enriched from newer asset state. This is compatible with the reference's
current-asset hydration model; the two sources need not be at the same block merely because they
contribute to one response.

An old transfer's sender/recipient differing from an embedded asset's current owner is not by
itself inconsistent: those fields describe different times. The previous owner/history example
was too broad. Likewise, history-derived sort/filter fields already materialized under D03 do
not require a history-tier read on every request.

Follow the pinned reference semantics when defining each endpoint. Any additional dependency
coordination must be justified by that endpoint's actual correctness requirements. Endpoint-specific
missing-data behavior, readiness reporting, and exceptional lag/recovery handling remain to be
defined. This discussion does not change accepted fork-recovery behavior under D05/D06.

## D06b — Serving during isolated projection failures

Status: Retryable unavailable by default, explicit plugin support for last-valid-state serving,
and its mandatory block number accepted on 2026-10-17. Detailed endpoint contracts remain open.

Return a retryable unavailable response by default when a contract-specific failure leaves a
required projection unable to satisfy an endpoint's serving contract. Apply this to the affected
endpoint/contract scope while unrelated indexing and queries continue under D13h/D15.

A plugin may explicitly support serving a preserved last valid view during such a failure, with
its allowed staleness/visibility behavior defined in the endpoint contract. That view must actually
be available and compatible; a checkpoint alone does not reconstruct it after partial writes.
This option cannot bypass the accepted query pause for known orphaned state during fork recovery.

If the plugin chooses that last-valid-state policy, every such response must expose the block
number through which the actual served view is complete for its declared projection scope. It
must not substitute the current chain head, an unrelated consumer's progress, or the last block
that changed an individual row. For example, a view complete through block 499 must report 499
even if its rows last changed at 420 and the chain is already at 520.

The block marker and returned data must refer to the same served view, including cache hits.
If the implementation cannot establish that relationship, it cannot claim to satisfy this
last-valid-state contract. Coverage metadata alone does not imply a single-block historical
snapshot across independently advancing partitions/projections; exact guarantees must remain
honest under the endpoint's declared consistency contract.

Ordinary processing lag continues to serve the latest eligible processed state. Ready compatible
generations keep serving during staged rebuilds under D13e. Reference-permitted missing enrichment
or partial optional data does not automatically make an endpoint unavailable; declared required
dependencies and the pinned API semantics determine what is needed to answer correctly.

Exact endpoint declarations, staleness limits, progress/readiness reporting, and retryable errors
within each API's compatibility contract remain open. This default serving policy does not impose
one shared snapshot or a common state/history block barrier.

The accepted metadata requirement is recorded in
[ADR 0036](../adr/0036-require-block-number-for-last-valid-state.md), and the default failure-serving
policy in [ADR 0039](../adr/0039-default-to-retryable-unavailable-for-failed-projections.md).

## D06c — Last-valid-state metadata transport

Status: Shared HTTP headers across plugins accepted on 2026-10-17. Exact header schema and
additional/non-HTTP metadata remain open.

Use shared Hyperion HTTP response headers for the mandatory served-view block number and
an explicit degraded/last-valid-state indicator. This gives plugins one host-level metadata
contract while preserving compatible JSON response bodies. Ensure browser clients can read the
headers. Cache the served-view block marker with its data rather than substituting current progress
on a cache hit; the last-valid-state indicator must accurately describe the serving mode.

Exact header names, whether block ID or other provenance fields are mandatory, handling of
responses with several projection scopes, and equivalent metadata for non-HTTP delivery remain
open. The default failure-serving policy is defined separately under D06b.

Rationale recorded in [ADR 0037](../adr/0037-shared-http-state-metadata-headers.md).

## D07 — Shared query interface and planning

Status: Shared structured query model accepted on 2026-10-17. Detailed query/compatibility and
execution contracts remain open.

D10 requires API domain plugins to use backend-neutral query interfaces. Use a shared
structured logical query model for plugin reads: typed filters, field selection, sorting,
pagination, and counts, with explicitly specified lookup/aggregation operations where required.
Plugins compose those logical queries and shape domain responses; backend adapters translate
and optimize execution using native operations. Native database query construction belongs in
adapters for this portable path.

Define the supported operation set and semantics explicitly. Required query capabilities must be
checked before plugin activation under D13b, alongside indexing/write requirements. An adapter
may choose different physical plans, including suitable materialized projections, while preserving
the query's observable semantics. This is an interface direction, not a promise to implement an
unrestricted database language or every native backend feature.

The internal query model does not by itself change public API request formats. Existing
`hyperion-history-api/src/api/routes/v2-state/get_table_rows/get_table_rows.ts` parses JSON filters
into MongoDB queries, then uses native count/find/sort/skip/limit operations. Reconcile its public
compatibility requirements explicitly when introducing the shared interface; do not silently
narrow the supported public behavior. Full pinned AtomicAssets/AtomicMarket parity remains
required under D02/D02a.

The precise operation vocabulary, types/comparison semantics, read isolation, pagination/count
contracts, cross-tier plans, and capability/version syntax remain open.

Rationale recorded in [ADR 0026](../adr/0026-portable-plugin-queries-through-adapters.md).

## D07a — Read consistency within one API request

Status: Per-endpoint snapshot policy accepted on 2026-10-17. Detailed read-view lifecycle and
adapter qualification remain open.

Use a shared read-snapshot capability that plugins explicitly require for endpoints whose
related reads must use one stable database view. For example, a matching-row count and a page of
results can use the same snapshot while indexing continues. Ordinary reads use the latest
eligible state without automatically sharing a snapshot across every query in the request.

The requirement belongs to the plugin's endpoint/query contract, not routine operator tuning.
Validate required snapshot support against the selected backend/adapter before activation under
D13b. Do not silently downgrade an endpoint's declared consistency guarantee.

This decision concerns related reads within the selected state backend. It does not add a shared
state/history block barrier or promise that different HTTP requests/pages share a snapshot.
Finality, fork-recovery query gating, and projection readiness remain applicable.

Existing Mongo table queries issue separate count/find operations without an explicit shared read
snapshot in the inspected route. MongoDB documents a separate
[snapshot read facility](https://www.mongodb.com/docs/manual/reference/read-concern-snapshot/);
adapter/deployment qualification and an equivalent WormDB guarantee remain to be established.

Read-view lifetime, resource limits, exact errors/retries, and handling of bound contexts during
forks or generation retirement remain open. Request/cache version binding is defined in D07b.
A database snapshot alone does
not establish common chain progress across independently advancing projections.

The operator accepted this per-endpoint approach as the current policy and noted that it can be
improved later. Do not infer a universal snapshot requirement for 4.5 or silently weaken a snapshot
guarantee that an endpoint actually declares.

Rationale recorded in [ADR 0027](../adr/0027-declare-snapshot-consistency-per-endpoint.md).

## D07b — Request and cache binding across cutovers

Status: Accepted for 4.5 on 2026-10-17. Context handles, cache keys, and retirement mechanics remain implementation design work.

Bind each state API request to one selected backend and a compatible set of required
projection generations/revisions for its lifetime. A backend cutover or live mapping activation
must not mix versions inside one response. Cached results must remain bound to the backend and
projection versions that produced them, with appropriate invalidation or versioned keys.

Complete reads using the bound context, or return an appropriate retryable error if that context
cannot remain valid. Known orphaned state remains subject to D06's fork-recovery query policy.
Keep D07a's per-endpoint read-snapshot policy and D06a's independent state/history progress.

Exact context handles, compatible-version selection, in-flight request draining/retry, cache identity,
retirement of old generations, and fork invalidation remain open.

Rationale recorded in [ADR 0051](../adr/0051-bind-state-requests-and-caches-to-compatible-versions.md).

## D09 — Delivery acknowledgment and replay correctness

Status: Delivery/replay contract accepted on 2026-10-17. Detailed recovery/source contracts remain open.

RabbitMQ remains the default transport under D04. The following contract governs replayed messages
and the point at which consumers may acknowledge completed work.

Use at-least-once delivery with idempotent processing. Consumers acknowledge a
message/batch only after the required effects are durably stored and restart progress is recoverable.
A crash after a write but before acknowledgment may cause redelivery; replay must not double-count
derived values or regress current state. Progress/checkpoints must not skip failed or only partially
applied work. Recovery may use backend-specific mechanisms as long as the declared guarantees hold.
State/history consumers maintain their own progress without imposing a global commit barrier.
Contract-specific failures remain subject to D13h's isolation and replay requirements.

Exact event/idempotency identity, batch/ordering semantics, checkpoint coordination, durability
levels, partial-failure handling, and source replay coverage remain to be defined. This contract
does not prescribe a distributed transaction across state/history stores or a disk sync per event;
batching and backend-specific durability mechanisms must preserve its guarantees.

### Source-confirmed acknowledgment bug to fix and verify

In `hyperion-history-api/src/indexer/workers/indexer.ts`, the Mongo route callback triggers
`ackAll`. The inspected `dynamic-table` path in `src/indexer/helpers/mongo-routes.ts` invokes that
callback from `Promise.all(promises).finally(...)`, which also runs when a bulk-write promise
rejects. This is an acknowledgment error: a failed batch reaches the success/acknowledgment path.
It contradicts the accepted contract. The implementation plan must correct and qualify that path,
including rejected and partially applied bulk writes and redelivery after a crash between write
completion and acknowledgment. This source inspection does not establish a particular live data-loss
incident. No implementation code was changed during architecture drafting.

Rationale recorded in [ADR 0021](../adr/0021-at-least-once-idempotent-indexing.md).

## D10 — API domain hosting and process layout

Status: AtomicAssets/AtomicMarket API hosting and first-party maintenance accepted on 2026-10-16.
Detailed process/module ownership and plugin distribution remain open.

The first-party AtomicAssets plugin includes AtomicAssets and AtomicMarket and is maintained by
EOS Rio. Its API domains register in Hyperion's existing Fastify API server and use backend-neutral
state/query interfaces. Keep domain-specific routing and behavior modular inside that plugin,
using the host's shared API facilities and the agreed contract-indexing interfaces.

These are officially maintained release components subject to the full-parity requirement and
pinned reference under D02/D02a. Their implementation is part of the 4.5 architecture, including
qualification of the supported backend combinations. Hosting in Fastify does not prescribe the
language or process location of every decoder, query engine, or backend implementation.

### Existing hosting mechanisms

Current source evidence:

- `hyperion-history-api/src/api/server.ts` hosts the existing Fastify API and loads custom plugin
  routes. `src/api/plugins.ts` registers shared API facilities such as Mongo/Redis connections,
  CORS, rate limiting, and documentation.
- `hyperion-tools/crates/light-api/src/main.rs` already hosts a separate Rust/Axum Light API over
  MongoDB. Existing work therefore includes more than one HTTP hosting model; choosing the 4.5
  default must account for reuse and operator deployment, not assume there is only one option.

Plugin distribution/versioning is defined next under D10a. Rust/Node integration mechanisms,
standalone tools, and complete module/repository ownership remain to be defined.

Rationale recorded in [ADR 0018](../adr/0018-eosrio-first-party-domain-api-plugins.md).

## D10a — First-party plugin release/version policy

Status: Accepted on 2026-10-17. Exact distribution, installation, and upgrade mechanisms remain open.

Hyperion already loads plugins from the `plugins` directory, gated by plugin installation state
and per-chain configuration (`src/indexer/modules/loader.ts`). The repository README identifies
`hpm` as its plugin manager and lists official EOS Rio plugins. First-party plugin maintenance
therefore builds on an existing extension/distribution model.

First-party plugin packages have versions/releases independent of Hyperion core. The combined
AtomicAssets/AtomicMarket package has one version and release under D10c.
Each Hyperion release identifies a tested, pinned set of compatible plugin versions so operators
have a supported default combination without selecting compatible versions manually. Domain fixes
may be released independently of a core release, with the applicable compatibility, capability,
and parity checks. Core and plugin versions are not required to advance in lockstep.

The release/install tooling must be able to identify the supported pinned combination. Exact
packaging, repository locations, installation defaults, upgrade selection, and compatibility-manifest
format remain open. Executable plugin upgrades are operator-initiated under D10d. Plugin updates
that change persisted mappings or runtime hooks remain subject to D13's activation/rebuild contracts.

Rationale recorded in [ADR 0019](../adr/0019-independent-plugin-releases-with-tested-pins.md).

## D10b — First-party plugin distribution and activation defaults

Status: Accepted on 2026-10-17. Exact distribution/build formats and install/upgrade commands
remain open.

The existing `hyperion-history-api/hpm` entry point invokes `src/cli/hpm.ts`. The inspected manager
contains Git clone/branch/pull and plugin build workflows; the loader separately checks installed
plugin state and per-chain enablement. D10a's tested compatible pins must be integrated with that
operator workflow; the current mechanisms alone do not qualify the new release policy.

Standard Hyperion 4.5 distributions include the tested, pinned AtomicAssets plugin, including
AtomicMarket. The plugin is disabled by default and enabled per chain through configuration. Operators
configure the required API domains and contract accounts without a separate plugin installation
or manual compatibility selection. Plugin code remains independently maintained/versioned under
D10a; inclusion in the standard distribution does not tie its release cycle to core.

Exact source/build/image distribution formats and install/upgrade commands remain open. Inclusion
does not itself activate a plugin's domain routes or indexing work for a chain; per-chain
configuration controls activation under the applicable capability and readiness checks.

Rationale recorded in [ADR 0020](../adr/0020-bundle-pinned-plugins-with-per-chain-opt-in.md).

## D10c — AtomicAssets plugin packaging

Status: Combined AtomicAssets/AtomicMarket plugin accepted on 2026-10-17. Detailed configuration
and distribution formats remain open.

Deliver one operator-facing AtomicAssets plugin containing both AtomicAssets and AtomicMarket
indexing and APIs. The community treats the AtomicAssets API as one system, including AtomicMarket;
operators should manage one plugin package, version, and release for it. Keep implementation modules
separate internally where useful, with indexing and API modules participating in their respective
runtime lifecycles.

The package releases independently of Hyperion core, is pinned as one tested compatible version
for each Hyperion release, and ships in standard 4.5 distributions disabled by default. Per-chain
configuration enables the plugin. Fine-grained internal API options remain to be defined without
requiring operators to install or coordinate separate AtomicMarket and AtomicAssets plugins.

This clarifies the earlier plural wording under D10/D10a/D10b: it does not require separate
AtomicAssets and AtomicMarket packages or release versions. The generic dependency system under
D15 still applies to other plugin relationships and can represent internal prerequisites. Full
parity for both API domains remains mandatory.

Rationale recorded in [ADR 0029](../adr/0029-combined-atomicassets-atomicmarket-plugin.md).

## D10d — Executable plugin upgrades and official catalog

Status: Accepted on 2026-10-17. Exact repository, metadata format, and tooling integration remain open.

Executable plugin upgrades are initiated by the operator. Hyperion's tested compatible version
selection supports that workflow; compatibility resolution or a catalog edit does not authorize an
unattended replacement of installed/running executable code. Upgrading Hyperion's standard
distribution may deliver its pinned plugins as part of that operator-initiated release upgrade.

EOS Rio provides the official plugin catalog as a repository where plugin compatibility is declared.
Keep that repository as the official catalog; this decision does not require a hosted registry or
marketplace service. Operators retain responsibility for auditing the plugins they choose under
D16b. The catalog works with D10a's tested release pins and D10b's standard distribution policy.

Automatic ABI-derived mappings within the configured account scope, live declarative mapping
reload, and dependency activation authorized by plugin enablement keep their accepted contracts.
They do not authorize installing/upgrading executable code because a contract ABI changed.

The catalog repository location/schema, declaration provenance and qualification requirements,
package references, and exact upgrade commands remain to be defined. SDK versioning follows D16c;
its exact compatibility-manifest fields remain open.

Rationale recorded in [ADR 0046](../adr/0046-operator-initiated-plugin-upgrades-and-repository-catalog.md).

## D11a — Backpressure and infrastructure outages

Status: Shared-reader infrastructure backpressure accepted on 2026-10-17. Exact budgets and
operational/recovery details remain open.

Hyperion already monitors worker queue sizes in
`hyperion-history-api/src/indexer/modules/queueMonitor.ts`. Reaching `max_queue_limit` or
`block_queue_limit` pauses readers; queues draining below `resume_trigger` permit resumption.
The monitor also provisions additional ingestor consumers within the configured autoscaling limit.
This is existing infrastructure to retain or adapt, not a new 4.5 queue-monitor component.

Retain existing bounded queue backpressure for genuine infrastructure outages or shared
capacity pressure. If Elasticsearch is unavailable long enough to exhaust its configured backlog
budget, pause the shared chain reader and resume after the queues drain to the configured resume
threshold. State processing
can proceed while its input remains available, but new state input may eventually pause with the
reader. Independent normal-path progress under D06a does not imply unlimited outage buffering.

This policy concerns infrastructure/capacity failures. Contract-specific
failures still follow D13h and must not be permitted to halt chain indexing through a blocked
shared queue. Exact byte/message/disk budgets, source coverage during pauses, alerting, and
recovery thresholds remain to be defined.

Rationale recorded in [ADR 0022](../adr/0022-retain-infrastructure-backpressure.md).

## D11b — Live and historical workload priority

Status: Live indexing priority accepted on 2026-10-17. Exact scheduling and resource-budget
mechanisms remain open.

Give live indexing priority by default when live work and historical backfill compete
for a chain's configured resources. Protect a minimum live-processing capacity and throttle bulk
work first under contention. Historical work can use spare capacity; a bulk-only profile can use
its full budget. Preserve the distinct live and historical execution strategies under D14a.

Existing live queues/workers provide a starting point but do not alone prove CPU, memory, storage,
or shared-pool capacity reservation. Exact budgets, scheduling, borrowing/reservation rules, and
operator overrides remain open. This policy does not waive infrastructure backpressure under
D11a or promise a fixed latency under insufficient capacity.

Rationale recorded in [ADR 0035](../adr/0035-prioritize-live-indexing-over-bulk-work.md).

## D12a — Backend availability in the 4.5 release

Status: Accepted.

The operator is open to implementing missing capabilities in WormDB as part of this effort. Required
work may therefore involve `P:\wormdb` as well as its Hyperion integration; existing capabilities are
not a fixed ceiling on the target architecture.

If implementing and qualifying the necessary WormDB capabilities requires too much effort, Hyperion
4.5 may launch with MongoDB as its only supported state backend. WormDB readiness must not prevent
that release once the other agreed 4.5 requirements are met. Permanent backend alternatives and
portable contract definitions remain the architecture target.

This is a conditional release decision, not a decision to defer WormDB now. The capability inventory,
qualification evidence, effort threshold, and decision point remain to be defined. The final API
surface and plugin-upgrade requirements are separate from backend availability.

In particular, full AtomicAssets and AtomicMarket parity under D02 remains mandatory when 4.5
launches with MongoDB alone.

Working Wire protocol-extension support under D18 is likewise required; the backend fallback does
not waive that launch requirement.

Rationale recorded in [ADR 0007](../adr/0007-allow-mongodb-only-45-release.md).

## D13 — Plugin architecture and protocol variations

Status: Included in Hyperion 4.5 scope; separation of extension interfaces accepted. Detailed contracts
and execution model remain open.

The operator requires contract-indexing definitions to be as dynamic as practical. The plugin system
must also evolve to support lower-level chain protocol changes, including the kinds of changes made
in [Wire Network's Hyperion fork](https://github.com/Wire-Network/hyperion-history-api).

This requirement spans `hyperion-history-api` and any corresponding ingest/decode paths in
`hyperion-tools`. The exact extension interfaces, lifecycle, implementation languages, and meaning of
"dynamic" will be defined through the architecture discussion after inspecting current extension
points and the Wire changes.

### Evidence informing the design

Inspected Wire reference: `Wire-Network/hyperion-history-api` at
`87b5e2e9647e0219fcf77d07b653668601e0ade7` on 2026-10-15. Its concrete protocol adaptations include:

| Variation | Observed adaptation | Architectural concern |
|---|---|---|
| Block timestamp epoch | Changed the timestamp codec epoch from 2000-01-01 to 2025-01-01 | A post-decode contract handler is too late to control timestamp interpretation |
| ABI extensions | Added enums; changed ABI table names to strings and added table/secondary-index metadata | ABI definition decoding and value codecs need explicit extension interfaces |
| KV table deltas | Added `contract_row_kv`; resolved tables by `table_id`; decoded big-endian keys and reconstructed scope/primary-key fields | Delta kinds, table identity, and key encoding cannot be limited to the legacy `contract_row` representation |
| Decoder dependencies | Updated native and JavaScript decoder implementations | Supporting a protocol variation may involve more than a TypeScript action/table handler |

Primary source changes:

- [Timestamp epoch](https://github.com/Wire-Network/hyperion-history-api/commit/b26c22d146).
- [ABI/table identity and KV routing](https://github.com/Wire-Network/hyperion-history-api/commit/8a3468e998).
- [Enum decoding](https://github.com/Wire-Network/hyperion-history-api/commit/e8fd3d0aca).
- [Native variant decoder update](https://github.com/Wire-Network/hyperion-history-api/commit/31922b5a43).

These are examples of variations to accommodate, not a claim that each is unsupported by every current
upstream decoder version.

The [Wire protocol investigation](hyperion-4.5-wire-protocol.md), refreshed on 2026-10-17 against
`wire-sysio` at `6508c9e8f4044df8d5acb082a7af3be6a80cd9a0`, adds node-level evidence for primary
and secondary KV streams, raw/composite keys, and enum/protobuf ABI metadata. An independent agent
also traced the fork's runtime and native submodules: differing schema paths and incomplete source
handling mean the fork is an extraction map, not a complete compatibility oracle. These are
source findings, not live acceptance results. D17 proposes the missing package/runtime boundary.

Current local extension points:

- `hyperion-history-api/src/indexer/modules/loader.ts` loads the configured parser and plugin
  action/delta/stream handlers. The parser is selected from a core-relative path. Plugin delta
  handlers receive decoded records.
- `hyperion-history-api/src/indexer/workers/deserializer.ts` owns the legacy protocol-delta handler
  map and decodes contract rows before invoking plugin delta handlers.
- `hyperion-tools/crates/core/src/blocks.rs` independently fixes the block timestamp epoch and slot
  interval in the Rust path. Extension design must cover this path as well as Node decoding.

### Accepted separation of extension interfaces

The plugin architecture has two explicit extension interfaces:

1. **Chain protocol adapter:** handles the chain's binary/ABI conventions, timestamp interpretation,
   delta kinds, and row/key identities. Produces a common event representation while retaining source
   bytes, protocol provenance, and identifiers needed to represent protocol-specific data faithfully.
2. **Contract indexing definition/plugin:** consumes that representation and defines contract-specific
   state/history projections, query fields and indexes, plus any required custom processing. The
   authoring model is defined in D13a; concrete execution mechanisms remain undecided.

The operator's rationale is maintainability through modularization. Protocol interpretation and
contract-indexing behavior must be able to evolve behind their respective interfaces. Contract
extensions consume the common event representation and do not implement timestamp or binary protocol
handling themselves. Process layout, package formats, and implementation languages remain open.

Rationale recorded in [ADR 0005](../adr/0005-separate-protocol-and-contract-extension-interfaces.md).

Subsequent decisions will define the declarative operation set, how executable codecs are distributed
to the Rust/Node paths, startup versus live loading, protocol activation across historical block ranges,
compatibility/version checks, plugin ordering, and backend-neutral projection/index contracts.

## D13a — Authoring dynamic contract-indexing definitions

Status: Accepted.

Contract indexing is declarative first, with executable code hooks for complex cases. Definitions
describe the contracts/actions/tables to consume, projections to maintain, and query fields/indexes
to expose. Specialized processors, such as AtomicData decoding or domain-specific market calculations,
may require code.

Definitions may be authored or generated automatically. Full auto-map mode under D13i removes the
need to author individual state mappings for allowed accounts; authored definitions/hooks remain
available for specialized indexing behavior.

The same contract-indexing definitions must work with MongoDB and WormDB. Adding or changing a
contract's indexing through these definitions and supported hooks must not require changes to
Hyperion core. A separate backend-specific copy of a contract definition does not satisfy this
requirement.

Definitions express the shared indexing/query requirements; the backend implementations provide the
corresponding storage and index behavior. The precise logical operation set and translation interface
remain to be defined; D14 addresses the projection-write ownership boundary. The chosen configuration
syntax must not dictate one backend's native schema.

D16 defines the default projection-hook runtime; D13c–D13e cover loading/activation. Configuration
syntax and precise state-read views remain open. D14 defines managed projection writes; D14a
addresses read/ordering semantics. Complex behavior may use hooks rather than fit a fixed
declarative vocabulary.

Rationale recorded in [ADR 0006](../adr/0006-portable-declarative-contract-indexing-with-hooks.md).

## D13b — Definition activation and backend capabilities

Status: Accepted.

Validate a definition's required capabilities against the selected backend and installed adapter
before activation. If those requirements cannot be satisfied, reject activation and identify the
missing requirement and the incompatible backend/adapter versions.

Backend-specific physical execution may differ where it preserves the declared behavior; whether
performance budgets are also required capabilities remains open. The operator is willing to add
missing WormDB capabilities where the effort is justified; D12a governs the release fallback if it is
too high.

## D13c — Loading and activation lifecycle

Status: Direction accepted on 2026-10-16; mandatory declarative mapping reload accepted on
2026-10-17 under D13d. Detailed lifecycle contracts remain open.

Prefer applying configuration and plugin/definition changes without restarts where practical.
Restarts of affected processes are acceptable depending on the area being changed. Hyperion 4.5
must extend the existing Mongo contract-configuration reload mechanisms to the mandatory mapping
scope in D13d. Executable hooks and native protocol components may still require controlled
restarts; other reload categories remain to be specified.

The earlier proposal to use controlled restarts as the general 4.5 approach is not accepted. Define
reload behavior by change category instead. In particular, changing a runtime setting, changing
persisted projection semantics, and replacing a protocol decoder are distinct operations.

Distribution, version identity, historical replay, coordinated activation, and changes to existing
projections must be specified alongside the lifecycle. D13d establishes the mandatory declarative
mapping reload scope; the supported runtime settings and plugin parameters remain open.

## D13d — Live configuration reload scope

Status: Target accepted on 2026-10-16; live reload of declarative contract mappings accepted as a
mandatory 4.5 launch requirement on 2026-10-17. Detailed reload/activation contracts remain open.

Hyperion 4.5 must support adding or updating declarative contract mappings while the indexer
continues running, including their declarative contract-indexing definitions. The operator identified
contract mappings as the most common changes. This is a release requirement, including for a
MongoDB-only launch under D12a, rather than an optional improvement limited to operational settings
and plugin parameters.

Validate changes before activation under D13b. Preserve the same definitions for both state backends.
Changes to persisted projection semantics must identify required rebuilding/backfill work; reload
alone must not imply that existing data has been regenerated. D13e defines staged replacement;
detailed activation mechanics remain open. Hooks and native protocol components may require
controlled restarts. The mapping reload requirement does not make live executable/native-code
replacement a launch requirement.

The original target is recorded in
[ADR 0008](../adr/0008-live-reload-contract-mappings-and-definitions.md); its mandatory launch status
and restart exceptions are recorded in
[ADR 0038](../adr/0038-require-live-declarative-mapping-reload-for-45.md).

## D13e — Mapping rebuilds and query serving

Status: Accepted on 2026-10-16; detailed activation and recovery contracts remain open.

When a mapping update requires new fields, indexes, or a changed projection, keep the previous
mapping/projection version serving while preparing the replacement. Lightweight changes that need
no backfill do not necessarily require a full rebuild.

Keep the previous version current with inputs it can correctly interpret under its ABI binding.
Validate the replacement, complete its required backfill and catch-up, and coordinate the serving
switch when it is ready. New query behavior becomes available with the ready replacement. For a
newly added contract with no previous version, publish readiness only once its declared initialization
requirements are met.

A deployment of an incompatible ABI does not make an old mapping valid for the new input format.
Continued serving of an existing projection and the freshness/coverage it can claim are separate
properties. The behavior during that incompatibility window remains open under D05/D06.

Independent mapping revision identity is defined in D13g. Source coverage, temporary resource budgets,
crash recovery during a rebuild, and the exact switch contract remain to be defined.

Rationale recorded in [ADR 0009](../adr/0009-bind-contract-mappings-to-abi-deployments.md).

## D13f — ABI deployment binding

Status: Accepted on 2026-10-16; exact deployment identity encoding remains open.

Contract mappings are versioned by the contract's ABI deployment. Historical indexing and replay
must select the mapping appropriate to the ABI deployment applicable to the data being processed,
rather than applying the latest installed mapping to every historical record.

Track the relevant chain, contract, and ABI-deployment identity. How deployments are identified,
ordered within a block where relevant, and handled across forks is part of the protocol/version
contract to be specified. Identical ABI content deployed twice must not automatically erase the
distinction between the two deployment events.

Rationale recorded in [ADR 0009](../adr/0009-bind-contract-mappings-to-abi-deployments.md).

## D13g — Mapping revisions within an ABI deployment

Status: Accepted on 2026-10-16; exact revision format and activation selection remain open.

A mapping has an independent immutable revision identity within its ABI-deployment binding. Its
logical identity is:

`chain + contract + ABI deployment + mapping revision`

This allows an indexing bug fix, added derived field, or improved index without requiring an on-chain
ABI redeployment. A correction produces a new mapping revision rather than modifying an existing
revision in place. D13e governs preparation and publication of rebuilt projections.

The revision format, hook/dependency version identity, active-revision selection, and the policy for
replaying original versus corrected results remain to be defined. Revision changes do not themselves
change the on-chain ABI deployment applicable to a source record.

Rationale recorded in [ADR 0010](../adr/0010-revise-mappings-independently-within-abi-deployments.md).

## D13h — Contract-specific failure isolation

Status: Accepted on 2026-10-16; detailed recovery and query contracts remain open.

Never halt chain indexing because of a contract-specific issue. Operators do not control smart
contract deployments or behavior; a contract issue is an expected condition that the indexing
architecture must isolate.

Continue source capture and unaffected contract/history indexing. Preserve replayable source data
within D20b's recovery-retention policy, the failure context, and affected processing positions.
Affected projections and their dependents must track their own incomplete progress; advancing the
shared chain cursor must not falsely mark
their data as processed.

Mark affected projections as waiting, lagging, or failed as appropriate. Do not interpret new data
using a known-incompatible mapping, and prevent contract-specific failures or retries from blocking
the shared pipeline indefinitely. When a compatible revision is available, validate it and catch up
the affected projections through the staged activation workflow.

The exact failure-isolation mechanism and retention budgets remain to be defined; D20b establishes
the bounded recovery window and missing-source policy. Query behavior
follows D06b's default retryable unavailability and explicit last-valid-state option; detailed
endpoint contracts remain open. Chain-wide protocol/source failures are a separate failure class
from contract-specific errors.

Rationale recorded in [ADR 0011](../adr/0011-contract-issues-never-halt-chain-indexing.md).

## D13i — Full auto-map mode and ABI evolution

Status: Accepted on 2026-10-16; discovery and activation mechanics remain to be defined. D13j defines
the accepted direction for physical index policy.

In full auto-map mode for state indexing, operators specify the contract accounts they allow.
Hyperion automatically builds and maintains the corresponding contract state mappings from their
ABIs. Operators must not need to author or approve a mapping for every table, field, or ABI deployment
within that configured scope. The auto-map engine must not expand the allowed account scope on its own.

Discover ABI-described state tables and their types for allowed accounts, generate mappings, and
respond automatically to ABI deployments. Bind generated mappings to their ABI deployments and give
changes immutable mapping revisions under D13f/D13g. Preserve the bindings required for historical
processing. Validate backend capabilities under D13b and use staged rebuilding where necessary under
D13e. Unsupported or malformed contract data follows D13h without halting chain indexing.

For an existing authored definition, automatically validate and carry it forward to a new ABI
deployment when its declared requirements remain satisfied. Reuse unchanged mapping content through
an explicit new deployment binding. Custom hook dependencies need an explicit compatibility contract.

Auto-generation derives structural state mappings from ABI/protocol information. Specialized domain
projections and calculations still use their declared definitions/hooks; an ABI alone does not
describe arbitrary contract business behavior.

### Existing mechanisms to extend

The current Mongo implementation already supports per-table `auto_index`, ABI-derived indexes,
manual index configuration, runtime contract-config reload, live state writes, and state synchronization.
Hyperion also stores ABI deployments and resolves historical ABI data by block. These mechanisms
are described in the [implementation baseline](hyperion-4.5-existing-mongodb.md).

The inspected dynamic-state selection/index-setup path is driven by configured contracts/tables;
index generation reads the current chain ABI. Extend and reconcile this foundation with account-scoped
discovery, independently versioned mapping revisions, staged activation, and the shared backend
contract. Do not describe historical ABI handling or Mongo config reload as entirely new 4.5 work.

Rationale recorded in [ADR 0012](../adr/0012-generate-state-mappings-for-allowed-accounts.md).

## D13j — Automatic index policy

Status: Direction accepted on 2026-10-16; exact defaults and configuration format remain open.

Mapping and storing a decoded ABI field is distinct from creating a physical query index for that
field. Full auto-map mode maps and stores all ABI-described fields, indexes row identity, and selects
additional physical indexes automatically using field types and query requirements within resource
budgets. Authored definitions may declare required indexes/query capabilities, which are validated
under D13b.

Operator configuration must be easy. Allowing accounts and enabling automatic state indexing should
be sufficient for ordinary use; selecting every field or tuning resource budgets must not be required.
Provide useful defaults and optional overrides for operators who need more control. The exact syntax
and any presets are not yet selected.

This extends existing Mongo `auto_index` and manual-index configuration rather than introducing
automatic index creation for the first time. The current master path creates an individual index
for each field of the configured table's struct/base structs. A resource-aware selection policy is
a change to reconcile with that behavior, not a capability established by this inspection.

The exact default selection policy, treatment of arrays/nested structures, query behavior without a
dedicated index, and budget/overflow behavior remain to be defined.

## D14 — Plugin/backend execution contract

Status: Managed-mutation ownership model accepted on 2026-10-17. Detailed operation and execution
contracts remain open.

D13a requires portable definitions with code hooks, D13b requires capability validation, and D09
requires idempotent application with durable completion before acknowledgment. Declarative mappings
and projection code hooks both produce logical effects whose application is owned by the shared
indexing runtime.

Accepted model:

1. A projection hook receives the source event and a backend-neutral context for required reads.
2. Declarative processing/code hooks produce or stage a batch of logical projection mutations.
3. The shared runtime validates and applies the batch through the selected backend adapter,
   coordinating durable completion, recoverable progress, idempotency, and fork/replay handling.

Plugins own the meaning of their projections and mutations. Core/runtime owns the generic
application lifecycle; adapters own native database operations and physical optimization. Native
MongoDB/WormDB calls belong in adapters for this portable projection path. Code-hook
projection effects participate in the same correctness contract as declarative effects.

Runtime ownership does not require one global writer or a cross-store commit barrier. Independent
projection progress and contract-failure isolation still apply. Exact mutation/read/query operations,
capability types, batch transaction/recovery guarantees, hook authoring syntax, and migration of
existing hooks remain open. This does not by itself require a database transaction per event or
settle the API query language. Protocol codec hooks, API route handlers, and notification side
effects have separate execution requirements.

Rationale recorded in [ADR 0023](../adr/0023-core-managed-projection-mutations.md).

### Existing integration evidence

`hyperion-history-api/src/indexer/modules/loader.ts` invokes decoded action/delta handlers and
`src/api/server.ts` gives API plugins the Fastify instance. Current Mongo state writes reside in
`src/indexer/helpers/mongo-routes.ts`. These are existing extension/storage points to reconcile with
the portable execution contract; this inspection does not establish that the SDK already implements
the accepted model.

## D14a — Projection ordering and parallel execution

Status: Ordering/parallelism policy accepted on 2026-10-17. Detailed sequencing, partition, and
bulk execution contracts remain open.

The shared runtime owns mutation application under D14 and preserves event order per projection
by default. State-dependent hook reads within an ordered processing unit account for earlier
staged changes. Observable results must match the intended source-event order; physical batching
and optimizations may differ while preserving those semantics.

Separate projections may progress concurrently when their declared dependencies permit. Plugins
may explicitly declare independent partition keys to permit parallel execution within a projection,
with order preserved within each partition. Generated state mappings may derive partition keys
from row identity when their operations are independent. Partitioning belongs to definitions/runtime
planning; normal operator configuration need not enumerate ordering keys.

### Live reading and historical bulk filling

Live reading can process independent contracts from the same block in parallel. Respect declared
cross-contract read/write dependencies; ordering is scoped to the projection/partition and does
not force a global serial lane for every contract in a block. Contract-failure isolation still
applies to affected projections and their dependents.

Historical bulk filling is a separate execution profile and may use more advanced parallelization,
batching, and reconstruction strategies than the live path. Its published projection results must
satisfy the same logical ordering, replay/idempotency, mapping-version, and API contracts. Existing
bulk tools/strategies should inform the implementation rather than forcing live scheduling onto
bulk work. Specific algorithms and publication/recovery mechanics remain to be defined.

Exact source-position ordering (including different event kinds and intrablock order), read-view
details, dependency declarations, partition validation, and repartitioning/replay rules remain
open. This does not introduce a same-block barrier for client API queries or a global chain-wide
serialization requirement.

Current-source evidence: `pushToDynamicTableQueue` distributes records over dynamic indexing queues
with a rotating counter; `setupIndexers` assigns separate ingestors; the Mongo route uses unordered
bulk writes. Those mechanisms do not establish projection/key ordering or ordered hook reads on
their own. The accepted contract needs explicit implementation and qualification rather than an
assumption that queue publication order guarantees final state order.

Rationale recorded in [ADR 0024](../adr/0024-ordered-projections-with-declared-parallelism.md).

## D14b — Mutation-batch atomicity

Status: Atomic-group policy accepted on 2026-10-17. Detailed transaction/read-view and recovery
contracts remain open.

An ordinary mutation batch is a unit of application/batching; it does not automatically
mean a multi-record transaction. Ordinary batches must preserve D09/D14a ordering, idempotency,
durable-completion, and restart-recovery guarantees. Partial application must remain safely
recoverable, and incomplete work must not be acknowledged as completed.

Definitions/plugins explicitly declare atomic groups when multiple records must commit
all-or-nothing within the selected state backend. Validate this requirement before activation under
D13b; an adapter must provide the declared guarantee or reject activation. The definition expresses
the requirement without depending on a native MongoDB/WormDB transaction API.

For example, a plugin may require a detail record and its summary counter to commit together.
Independent row replacements may use ordinary batches. Live and bulk execution may choose their
physical batching strategies while preserving any declared atomic groups.

The precise atomic-group boundary, supported operations, isolation/read-view guarantees, and
checkpoint/recovery protocol remain open. Atomic commit alone does not define a consistent snapshot
across several API reads. This decision does not require a transaction across independent
projections, Elasticsearch, or other stores.

Rationale recorded in [ADR 0025](../adr/0025-explicit-atomic-projection-groups.md).

## D14c — Replay determinism of projection hooks

Status: Direction accepted on 2026-10-17, qualified by the operator's "if possible." Feasibility,
enforcement, supported coverage, and any additional launch obligation remain open.

Pursue projection hooks that produce deterministic logical effects from versioned source inputs,
the applicable ABI/mapping/hook revision, and declared prior projection state. The intended contract
is that live processing, restart recovery, and historical rebuilding produce equivalent persisted
results, independent of uncaptured wall-clock time, randomness, or changing network responses.

External enrichment that affects persisted projection results would need to become a captured,
versioned input or run outside this projection path. This proposal concerns indexed projection
effects; API handlers and notification delivery have separate contracts. Exact SDK restrictions,
input-capture mechanisms, and enforcement/qualification remain open. The operator's qualified
agreement does not establish an unconditional guarantee for arbitrary executable hooks or accept a
new universal sandbox requirement for the 4.5 launch. Existing idempotency, logical live/bulk
equivalence, and fork-recovery obligations remain in force.

Current `hyperion-history-api/src/indexer/modules/loader.ts` imports installed/enabled plugins as
Node modules and calls action/delta handlers directly. That source observation does not establish
deterministic execution or an untrusted-code sandbox. The execution trust model is a separate
decision under D16b; exact isolation/process placement remains open under D16a.

Rationale recorded in [ADR 0044](../adr/0044-target-deterministic-replay-safe-projection-hooks.md).

## D15 — Plugin dependencies and activation

Status: Automatic indexing-dependency activation accepted on 2026-10-17. Detailed dependency and
lifecycle contracts remain open. AtomicAssets packaging is clarified separately under D10c.

Plugins declare the projections/query contracts they require and their compatible
versions. When an operator enables a plugin for a chain, resolve and activate its required indexing
dependencies automatically within the operator's configured contract scope. Dependencies must not
silently expand an account allowlist. API domain activation remains explicit: requiring another
plugin's indexed data does not automatically expose that plugin's public routes.

Core/runtime validates the dependency graph and compatible installed versions/capabilities before
activation. Use the Hyperion release's tested plugin set where applicable. If a prerequisite is
missing or incompatible, reject the dependent activation with an actionable explanation while
unrelated chain indexing continues. Dependent serving must satisfy prerequisite readiness;
this does not impose a common-block barrier across independently advancing projections.

The inspected loader uses plugin installation state and per-chain enabled flags to load handlers;
it also collects plugin-declared dynamic contracts. These paths are existing integration points,
not evidence of the accepted dependency-resolution/readiness contract. The external plugin SDK's
full behavior has not been established by this inspection.

Dependency declaration syntax, optional dependencies, cycle handling, resolved-plan inspection,
disabling/upgrading dependencies, and precise readiness/error behavior remain open.

Rationale recorded in [ADR 0028](../adr/0028-activate-required-indexing-dependencies.md).

## D16 — Contract projection hook runtime

Status: JavaScript execution with TypeScript authoring accepted as the 4.5 default on 2026-10-17.
Detailed execution-isolation and SDK contracts remain open.

This decision applies to contract projection hooks. The investigation in D13/D17 establishes the
separate protocol-provider requirements for chains such as Wire. The default hook language does
not establish a universal plugin language or dictate codec implementations in Node/Rust consumers.

Use JavaScript execution in Hyperion's Node.js runtime, with TypeScript authoring, as the
standard 4.5 model for executable contract projection hooks. Hooks use the shared SDK's source
event, read/query, and managed mutation interfaces; their definitions remain portable across
MongoDB and WormDB. Build on the current plugin authoring/loading model while extending its
execution contract under D14.

The inspected `hyperion-history-api/src/indexer/modules/loader.ts` imports plugin
`build/index.js` modules and registers action/delta handlers. This establishes an existing
JavaScript integration path, not an implementation of the accepted managed hook SDK contract.

Protocol codecs, native decoding/acceleration, and backend implementations retain their separate
interfaces; their implementation languages
and integration mechanisms are not prescribed by this hook-runtime choice.

Worker/process isolation, resource limits, failure supervision, hook replacement, native bindings,
and exact SDK/build/version contracts remain open.

Rationale recorded in [ADR 0033](../adr/0033-default-javascript-typescript-projection-hooks.md).

## D16a — Worker infrastructure evolution and hook isolation

Status: Direction accepted on 2026-10-17 — build on the existing worker infrastructure. Precise
hook isolation, scheduling, and process/pool placement remain open.

The operator reports that the existing worker infrastructure has been working well and supports
improving it. Evolve that infrastructure as the 4.5 baseline. Changes should provide the agreed
projection ordering, backend-neutral execution, durable progress/recovery, and contract-failure
isolation while preserving useful existing scheduling and operational behavior.

Current source mechanisms to retain or extend:

- `master.ts` provisions separate live/history reader/deserializer roles, indexing roles, and a
  deserialization pool, launching processes with `cluster.fork`.
- `updateWorkerAssignments` assigns contracts across the pool using observed contract-usage
  counts; contract-aware assignment is existing behavior, not wholly new 4.5 work.
- `handleWorkerDisconnect` relaunches workers and reinitializes deserializer ABI state.
- `queueMonitor.ts` provides queue thresholds, pause/resume, and bounded ingestor autoscaling.
- Projection handlers currently run through direct module-loader calls inside the calling worker.

These are source observations, not current qualification of all new hook-job isolation, ordering,
timeout, and recovery contracts. Track incomplete work by projection under D13h/D09 and preserve
declared partition ordering. A separate supervised hook pool is still an implementation option;
the operator's direction does not accept that particular new stage or require dedicated processes
per plugin/contract. Qualify process/queue boundary changes with representative throughput and
failure/recovery evidence.

Fastify API handlers and pre-decode protocol providers retain their own execution requirements.
Exact worker reuse, job/IPC boundaries, resource limits, supervision, and restart scope remain open.

Rationale recorded in [ADR 0034](../adr/0034-evolve-existing-worker-infrastructure.md).

## D16b — Executable plugin trust model

Status: Accepted on 2026-10-17. Exact SDK, isolation, and qualification mechanisms remain open.

Treat operator-installed executable plugins as trusted code for 4.5, including their projection
hooks. Operators are responsible for auditing the plugins they decide to use. Use shared SDK
contracts and conformance/replay qualification for supported behavior; defer a hardened environment
for executing untrusted plugin code as separate work.

This retains the existing Node module-loading trust baseline without deciding exact worker
placement. Contract-failure isolation, replay/idempotency correctness, and resource/recovery
requirements still apply. Automatically mapping an allowed on-chain account does not authorize
installing or executing arbitrary off-chain plugin code; contract data remains input to validate.

Exact SDK restrictions, process/resource isolation, and conformance mechanisms remain open. Trust
in installed code is distinct from guaranteeing that arbitrary code cannot access a clock, network,
filesystem, or other runtime facilities.

Rationale recorded in [ADR 0045](../adr/0045-trusted-plugins-and-operator-audit-responsibility.md).

## D16c — Shared plugin SDK compatibility and versioning

Status: Accepted on 2026-10-17. Initial stable version, migration, support lifetimes, and manifest
details remain open.

Version the shared plugin SDK contract independently of Hyperion core and individual
plugin releases. Keep changes backward compatible within an SDK major version; breaking interface
or semantic changes require a new SDK major. Plugins declare the SDK/interface versions they target,
and core declares the versions it supports, reflected in compatibility metadata under D10d.

This policy complements capability checks and tested first-party release pins, without making
a version declaration alone proof of parity or runtime correctness. Migration from the existing
`@eosrio/hyperion-plugin-core` package, the initial stable major, supported-version lifetimes, and
exact manifests remain open. Native protocol-provider/module ABI compatibility retains its separate
contract under D17.

Rationale recorded in [ADR 0047](../adr/0047-independent-sdk-versioning-and-major-compatibility.md).

## D17 — Chain protocol extension package and runtime providers

Status: Single operator package with coordinated internal components accepted on 2026-10-17.
Detailed implementation mechanisms remain open.

Use one versioned chain extension package, such as a Wire package, that can supply a
protocol profile, compatible codec providers, normalization rules, and optional contract/API
modules. Keep protocol and contract interfaces separate under D13 even when they share a package.
Providers may use JavaScript, Rust, or native code as appropriate; the standard authoring language
of projection hooks does not dictate the implementation language of protocol codecs.

Operators manage one package/version and one installation, upgrade, and activation workflow.
Package tooling resolves and verifies the required components for the applicable runtimes/platforms;
operators must not separately install or match Node and Rust pieces. A package release identifies
a compatible internal component set. Validate the required set before activation, and report an
incomplete/incompatible package clearly rather than treating it as ready.

Select/register the required providers before interpreting the SHiP handshake, blocks, binary
contract ABIs, and row values. Cover the relevant Node and independent Rust live/bulk/offline
consumers with the same declared protocol semantics. Merely loading a JavaScript API plugin or
changing one Node native addon does not configure every decoder path.

Normalization must accommodate the protocol's complete row/index identity and source metadata.
Do not force a raw/compound KV key into a lossy legacy scope/uint64-primary-key representation.
Retain raw bytes and deployment/protocol provenance for correct historical interpretation and
replay. Portable projections and queries continue through the selected MongoDB/WormDB adapter.

Native codec changes may require rebuilt artifacts and controlled restarts of affected workers;
the accepted live-reload target for mappings does not require live native-code replacement.
Compatibility must be qualified against a pinned node/protocol baseline and applicable Node/Rust
fixtures. See the [Wire source evidence and qualification limits](hyperion-4.5-wire-protocol.md).

The exact module ABI, bindings, static/dynamic loading, IPC/WASM choices, manifests, provider
version/activation rules, supported Wire feature baseline, and release acceptance scope remain
open. This is not an accepted decision to implement every latest Wire feature as a 4.5 launch gate.

Rationale recorded in [ADR 0030](../adr/0030-unified-chain-extension-package.md).

## D18 — Wire as a protocol-extension acceptance case

Status: Working Wire support accepted as a mandatory 4.5 launch requirement on 2026-10-17.
Detailed compatibility baseline and acceptance contracts remain open.

Deliver a working Wire chain extension as a 4.5 release requirement and the concrete
acceptance case for D13/D17. Qualify applicable live ingestion and historical replay paths against
an explicitly agreed, pinned node/protocol baseline. Demonstrate protocol adaptation through the
operator-managed package, not only interface definitions or isolated decoder examples.

The supported Wire feature baseline, exact node/codec versions, fixtures, end-to-end scenarios,
and required runtime/backend combinations must be agreed separately. The source-investigation
commits are not automatically that support baseline. This decision concerns protocol-extension
qualification; it does not define additional client API parity obligations.

This requirement applies even if 4.5 launches with MongoDB as its only state backend under D12a.
The architecture/research documents and source inspection do not establish that it has been met.

Rationale recorded in [ADR 0031](../adr/0031-require-working-wire-support-for-45.md).

## D18a — Wire compatibility baseline

Status: New Wire testnet with Wire contracts accepted as the target on 2026-10-17. Exact build,
contract, and extension pins remain open.

The operator states that the target testnet does not yet exist and will be launched with Wire's
contracts. Qualify the mandatory Wire extension against that testnet launch's selected node/protocol
build and deployed contract set. Record exact source revisions and deployed WASM/ABI artifacts,
the extension/component versions, and the network/genesis identity when preparing the qualification
profile and fixtures. The contract inventory and detailed test scenarios remain to be defined.

This is a planned acceptance target. Testnet launch, plugin implementation, and qualification are
outstanding; the architecture decision does not establish a running deployment.

GitHub API verification on 2026-10-17 found the published
[Wire v1.0.0 release](https://github.com/Wire-Network/wire-sysio/releases/tag/v1.0.0), published
2026-07-30, with tag commit `7dede884e0150d36fa788e85a119e10653ade8ca`. Its release notes explicitly
limit general-audience usage; a published tag is not evidence of a particular deployed network build.
The source investigation used newer master commit `6508c9e8f4044df8d5acb082a7af3be6a80cd9a0`,
committed on 2026-10-16. Neither is automatically the accepted support target.

The testnet launch profile determines the compatibility pin. Public release and investigation refs
remain inputs to selecting that profile. Required historical protocol eras and later upgrade policy
remain open; a moving branch must not silently expand the launch scope.

Rationale recorded in [ADR 0032](../adr/0032-qualify-wire-on-new-testnet-with-wire-contracts.md).

## D19 — Upgrade compatibility for existing deployments

Status: Accepted as a mandatory 4.5 release requirement on 2026-10-17. Supported source releases,
index schemas, and detailed migration/qualification contracts remain open.

Supported existing deployments must be able to upgrade to 4.5, keep their Elasticsearch history
usable, and resume indexing without a mandatory full history reindex. Initialize or rebuild new
or changed state projections separately where needed; preserving history does not imply every
existing state projection already meets the new plugin/backend contracts.

Implementation agents must preserve this requirement when changing history schemas, readers,
writers, and plugin integrations. A separate state initialization/backfill job does not authorize
requiring a full Elasticsearch history reindex. Release qualification must demonstrate that
pre-upgrade history remains queryable and indexing resumes with the retained history.

Current source uses `settings.index_version` when constructing Elasticsearch write destinations
(`src/indexer/helpers/elastic-routes.ts`) and history read targets (for example,
`src/api/routes/v2-history/get_transaction/get_transaction.ts`). This provides existing index
selection mechanisms, not evidence that 4.5 schema, query, or restart compatibility is qualified.

Supported source releases/index schemas, required migrations, configuration conversion, rollback,
and acceptance evidence remain open. Do not infer a particular prior release baseline or a
completed migration from this requirement. It does not by itself require a zero-downtime upgrade.

Rationale recorded in [ADR 0040](../adr/0040-preserve-history-without-full-reindex-on-45-upgrade.md).

## D20 — Projection initialization and source coverage

Status: Accepted on 2026-10-17. Detailed declarations, source interfaces, and initialization job
contracts remain open.

Contract-indexing plugins must explicitly declare each projection's supported initialization
strategies and required source coverage. A snapshot plus forward catch-up may suffice for a current-state
projection; history-dependent projections may require earlier events or a separately qualified
seed. Check that available sources satisfy the declared requirements before marking a projection
ready under D13e. Operators should not have to infer these requirements from plugin internals.

Existing mechanisms include the portable snapshot loader in `crates/snapshot-load` and RPC-based
contract-state synchronization in `hyperion-history-api/src/cli/sync-modules/sync-contract-state.ts`.
Their existence does not establish complete initialization/catch-up semantics for every projection.
Snapshot contents, selected history records, and replayable source events are not interchangeable
without qualifying the data and coverage a projection actually needs.

The exact declarations, source interfaces, chain/block identity checks, snapshot-to-live handoff,
coverage-gap handling, and resumable job lifecycle remain open. D19 preserves existing history
without assuming that it contains every input needed to build a new state projection.

Rationale recorded in [ADR 0041](../adr/0041-require-projection-initialization-and-coverage-declarations.md).

### D20a — Initialization and rebuild job ownership

Status: Accepted on 2026-10-17. Detailed job interfaces and execution mechanisms remain open.

The shared Hyperion runtime owns initialization/backfill/rebuild job scheduling,
durable progress, retry/resume, and readiness publication. Plugins declare the required inputs,
coverage, dependencies, and initialization strategies and supply domain-specific transformations
through the shared contracts. Plugins should not each need to implement their own independent
job/recovery infrastructure.

Build on existing workers and tools under D16a, preserving live indexing priority under D11b and
isolating contract-specific failures under D13h. This decision does not select a new process pool,
require a separate orchestration service, or impose a global state/history commit barrier. Exact
job interfaces, checkpoint mechanics, and process placement remain open.

Rationale recorded in [ADR 0042](../adr/0042-shared-runtime-orchestrates-projection-initialization-and-rebuilds.md).

### D20b — Recovery source retention and unavailable ranges

Status: Accepted on 2026-10-17. Exact budgets, source selection, and retention mechanisms remain open.

Use a bounded, configurable local recovery window for isolated projection failures. Retaining
input for a broken contract must not consume unbounded local storage or eventually halt unrelated
chain indexing. Use configured historical sources for older inputs beyond that local window;
their actual input types and ranges must satisfy the projection's declarations.

If required input is no longer available from any configured source, report the affected projection
as incomplete and requiring source data/backfill or a qualified reinitialization strategy. Do not
silently skip a gap, advance its completed progress, or claim readiness. A retained checkpoint is
not proof that replay data still exists. Unrelated queries and indexing continue under D13h/D06b.

Exact budgets, expiration/eviction rules, source selection, operator visibility, and how to preserve
recoverable work within the declared retention guarantees remain open. This policy concerns isolated
contract failures; D11a's shared-reader backpressure for infrastructure outages and D05's required
fork rollback still apply.

Rationale recorded in [ADR 0043](../adr/0043-bound-recovery-retention-and-report-unavailable-inputs.md).

## D21 — Lossless sequence identifiers on every chain

Status: Accepted on 2026-10-20 UTC. Implementation and release qualification remain open.

Use canonical decimal strings for uint64 sequence identifiers and aliases across every supported
chain, including Antelope and Wire. Apply the same exact semantics through queries, caches, streams,
archives and the applicable Node/Rust paths. The operator explicitly rejected limiting this change
to Wire, so preserving numeric Antelope sequence responses is superseded as a proposal. This is an
intentional 4.5 public response-type change requiring client migration documentation.

Preserve the original identity rather than rounding through JavaScript Number. This decision does
not stringify uint32 code/ABI counters, blocks, ordinals or unrelated counts. Derived count overflow
and unresolved legacy parent-filter semantics remain separate. Existing Elasticsearch history stays
usable under D19; additive exact coverage and qualified recovery must not become a mandatory full
history reindex. Rounded retained values alone cannot establish exact sequence coverage.

Rationale and qualification requirements are recorded in
[ADR 0053](../adr/0053-use-lossless-sequence-strings-on-all-chains.md). Implementation is tracked in
[History API #219](https://github.com/eosrio/hyperion-history-api/issues/219).
## Supporting documents

- `benchmark/atomicassets/REQUIREMENTS.md` — existing requirements and two-track plan.
- `hyperion-45-vision.html` — existing architecture presentation.
- `benchmark/atomicassets/FRESHNESS_OVERLAY_DESIGN.md` — experimental live-engine design and measurements.
- `benchmark/atomicassets/V2_COMPAT_MATRIX.md` — version compatibility research and known domain changes.
- [Wire protocol investigation](hyperion-4.5-wire-protocol.md) — pinned node/fork source evidence,
  integration gaps, proposed extension package boundary, and qualification limits.

The domain glossary is in `CONTEXT.md`; consequential trade-offs are recorded in `docs/adr/`.
