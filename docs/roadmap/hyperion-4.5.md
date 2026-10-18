# Hyperion 4.5 development roadmap

Status: Repository names and initial delivery breakdown approved on 2026-10-17.
The milestones organize the accepted architecture; implementation and qualification are pending.
Published repositories and issues are tracked in the publication record below.

The [architecture definition](../architecture/hyperion-4.5.md) is the release contract.
This roadmap turns it into verifiable delivery milestones. Component designs belong in the
work that implements them; another round of detailed architecture interviews is not required
before starting.

## Repository ownership

Use one release coordination issue in hyperion-tools, with implementation issues in the repository
that owns the change. Cross-repository slices have one lead issue and linked PRs; split follow-up
work when it becomes independently deliverable. Do not duplicate the release backlog in every repo.

| Repository | Responsibility | Repository action |
|---|---|---|
| eosrio/hyperion-tools | Release plan, Rust ingestion/codec and snapshot tooling, reusable processing/recovery components where appropriate, replay and cross-component qualification. | Existing; retain as the main coordination home. |
| eosrio/hyperion-history-api | Existing workers and RabbitMQ integration, API host, Elasticsearch compatibility, operator configuration and extension activation. | Existing; substantial 4.5 changes required. |
| eosrio/hyperion-sdk | Independently versioned public extension contracts, TypeScript authoring support, conformance fixtures, examples and migration guidance; required language bindings share defined semantics. | Approved new repository; bootstrap tracked below. |
| eosrio/hyperion-plugins | Official catalog metadata and declared compatibility with tested release pins. | Reuse the existing catalog repository. |
| eosrio/hyperion-atomicassets-plugin | One operator package for AtomicAssets and AtomicMarket indexing and API parity, with modular internals. | Approved new repository; bootstrap tracked below. |
| eosrio/hyperion-wire-plugin | One operator package for Wire protocol support, including coordinated Node/Rust components. | Approved new repository; bootstrap tracked below. |
| igorls/wormdb | Database engine capabilities and engine-specific correctness/performance fixes. | Existing; file engine work here when demonstrated necessary. |
| eosrio/hyperion-docs | Operator-facing upgrade, installation, configuration and recovery documentation. | Existing; publish operator guidance as behavior becomes qualified. |

Backend adapters remain separately versioned extension packages. Their initial source homes should
follow the implementation language and host integration; assign those homes in the foundation
work before filing adapter implementation issues. A package does not automatically need another
repository. Repository layout must not introduce a new service or a worker rewrite by itself.

### Existing SDK and catalog migration

The current History API dependency is @eosrio/hyperion-plugin-core ^0.1.0 from
[eosrio/hyperion-plugin-core](https://github.com/eosrio/hyperion-plugin-core).
Its action/delta/stream handlers and Fastify route integration are migration inputs for the new SDK.
Explicitly document supported legacy behavior, necessary migrations and unsupported combinations.
Do not silently replace the old package or assume all existing plugins already satisfy new contracts.

The existing [catalog](https://github.com/eosrio/hyperion-plugins/blob/main/catalog.json) contains
SimpleAssets and Telos EVM entries. Introduce compatibility metadata with validation and a documented
legacy-entry policy. Verify consumers before changing the catalog format. A catalog update must
not upgrade executable code automatically.

## Delivery milestones

These are outcome milestones, not promises that each fits in one agent session. Publish small
implementation slices as their immediate prerequisites become clear.

| Milestone | Observable result | Main dependencies |
|---|---|---|
| M0: Release foundation | Architecture baseline is published; repository/package ownership, SDK migration approach, supported upgrade baselines and qualification matrix are recorded. New repository scaffolds and catalog validation have maintainers and CI. | Repository naming/ownership review. |
| M1: First portable MongoDB path | One allowed contract is indexed through the shared interfaces using existing workers and queried through Hyperion, with backend-neutral definitions/queries, durable retryable processing and a real MongoDB fixture. | Minimum SDK and adapter contracts; no full SDK freeze required. |
| M2: Recoverable dynamic projections | The first path survives replay, restart and fork replacement. A declarative mapping changes without restart, with ABI-bound revisions, initialization coverage and validated activation. Contract failures remain isolated. | M1; extend existing reload/sync mechanisms. |
| M3: Required domain and protocol support | AtomicAssets plus AtomicMarket pass the pinned parity suite. The Wire package works across applicable Node/Rust paths and passes the selected new testnet profile. | SDK capabilities grow through real slices; Wire profile and parity inventory start in parallel with M1. |
| M4: Release qualification and operator distribution | A retained-history upgrade, pinned component distribution, recovery exercises and operator workflows pass together for the supported matrix. Backend qualification decision and known limits are published. | All mandatory outcomes; qualified MongoDB is sufficient if WormDB is deferred. |

Existing history compatibility and reliability checks run from the beginning. They are not
postponed until M4. Wire and AtomicAssets work should expose missing generic capabilities early,
instead of waiting for a speculative complete SDK.

## Parallel workstreams and release gates

- **Upgrade and reliability:** preserve usable existing Elasticsearch history and continued history
  indexing without a full reindex; initialize new state separately. Exercise failure/retry,
  reconnect, backpressure and live-priority behavior using the existing worker infrastructure.
- **Projection lifecycle:** add portable reads/writes incrementally, then initialization/resume,
  bounded recovery coverage, ABI deployment and mapping revisions, auto-map of allowed accounts,
  staged live reload and per-projection ordering/declared parallel partitions. Bulk and live paths
  must produce equivalent logical results.
- **Fork and serving correctness:** prove canonical rollback and restart recovery, reversible-head
  default and optional irreversible-only mode. Integrate affected-query pauses, retryable errors,
  explicitly supported last-valid responses with completeness headers, endpoint consistency and
  request/cache version binding. Storage rollback alone does not complete this gate.
- **AtomicAssets:** inventory and qualify the complete AtomicAssets and AtomicMarket API surface
  against atomicassets/atomicassets-api 2.7.0 at commit
  0aabc47dcf8bd0804d4dcecb336c90fc02a9c89e. Slice endpoint families with their required projections,
  filters, sorting, counts and pagination. One successful endpoint or snapshot import is not parity.
- **Wire:** select exact node/contracts/genesis/build inputs, implement the protocol provider before
  affected decoding, and qualify both required language paths, live/bulk input, key preservation
  and recovery. Investigation revisions are not accepted launch pins. The new testnet must be
  launched with Wire's contracts before final live qualification.
- **WormDB:** assess required capabilities early and file demonstrated engine gaps in WormDB.
  Qualify the unchanged definitions and plugin queries against the same applicable conformance,
  recovery and parity requirements. If the effort is too high, record an explicit MongoDB-only
  release decision; permanent backend support remains the architecture direction.
- **Distribution and operator lifecycle:** independent SDK/plugin/adapter releases, tested versions
  pinned for Hyperion, bundled first-party plugins disabled by default, catalog compatibility checks,
  one-package Wire installation and controlled executable updates. Qualify backend cutover after
  destination initialization/catch-up/validation; a brief state API pause is permitted.

Do not schedule transport replacement, an untrusted-code sandbox, a wholesale worker-pool rewrite
or zero-downtime backend cutover as prerequisites for 4.5. Revisit only with a concrete requirement
or measurement and a separate scope decision.

## First issue batch

The [initial issue drafts](hyperion-4.5-initial-issues.md) define ten bounded starting outcomes:

1. Establish SDK/plugin repositories and upgrade the existing catalog.
2. Define the supported release and upgrade qualification matrix.
3. Preserve retryability when MongoDB writes fail.
4. Prove an upgrade keeps existing Elasticsearch history usable.
5. Round-trip one contract through the shared SDK and MongoDB adapter.
6. Serve that contract through the existing Hyperion workers and API.
7. Prove durable projection rollback and restart recovery.
8. Define the Wire testnet qualification profile.
9. Assess WormDB against required backend capabilities.
10. Establish the pinned AtomicAssets/AtomicMarket parity inventory.

The following already exist and must be reused rather than duplicated:

- [history-api #188](https://github.com/eosrio/hyperion-history-api/issues/188): fork cleanup after
  failed block lookup. Reproduce and resolve against the chosen 4.5 baseline.
- [history-api #187](https://github.com/eosrio/hyperion-history-api/issues/187): startup, AMQP,
  backpressure, health and catch-up reliability reports. Separate confirmed correctness defects
  from proposed performance work before selecting implementation slices.

The failed-Mongo-write acknowledgment path is source-confirmed in the inspected working tree.
The reports in #187/#188 have not all been reproduced during this planning exercise.

## Publication and evidence

The operator approved the repository names and first batch on 2026-10-17. Publish the
architecture/roadmap baseline and a release coordination issue; create the approved component
issues with real cross-repository references and blocking links. Investigation/setup issues must
not be labeled implementation-ready merely because they are on the roadmap.

All implementation issues cite the published architecture baseline, state their delivered behavior,
and define acceptance evidence and genuine blockers. Milestones are complete only when their
integration and operator outcomes pass; a source inspection, unit test or benchmark alone is not
release qualification. Estimate dates after the first slices establish effort and available capacity.
