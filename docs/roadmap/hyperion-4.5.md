# Hyperion 4.5 development roadmap

Status: Repository names and initial delivery breakdown approved on 2026-10-17.
The milestones organize the accepted architecture; implementation and qualification are pending.
Published repositories and issues are tracked in the publication record below.

The [architecture definition](../architecture/hyperion-4.5.md) is the release contract.
This roadmap turns it into verifiable delivery milestones. Component designs belong in the
work that implements them; another round of detailed architecture interviews is not required
before starting.

Hyperion 4.5 requires **Node 24 or newer**, with maintained CI on **Node 24 and 26**.
[The runtime baseline transition](https://github.com/eosrio/hyperion-tools/issues/65) covers the
History API/MongoDB adapter, SDK, Wire package, stream client and catalog tooling. Preserve historical
Node 22 evidence at its original versions; current runtime and release acceptance must use the
selected 24/26 baseline. This does not activate a production runtime upgrade.

## Repository ownership

Use one release coordination issue in hyperion-tools, with implementation issues in the repository
that owns the change. Cross-repository slices have one lead issue and linked PRs; split follow-up
work when it becomes independently deliverable. Do not duplicate the release backlog in every repo.

| Repository | Responsibility | Repository action |
|---|---|---|
| eosrio/hyperion-tools | Release plan, Rust ingestion/codec and snapshot tooling, reusable processing/recovery components where appropriate, replay and cross-component qualification. | Existing; retain as the main coordination home. |
| eosrio/hyperion-history-api | Existing workers and RabbitMQ integration, API host, Elasticsearch compatibility, operator configuration and extension activation. | Existing; substantial 4.5 changes required. |
| eosrio/hyperion-stream-client | Client protocol, lifecycle and packaging; qualify actual client/server compatibility with the History API. | Existing; track server integration in History API #231. |
| eosrio/hyperion-explorer | Companion explorer frontend, dependency/toolchain upgrades, browser UX, and client compatibility with the 4.5 API and stream contracts. | Existing; modernization started locally; publish implementation and qualify against pinned 4.5 components. |
| eosrio/hyperion-sdk | Independently versioned public extension contracts, TypeScript authoring support, conformance fixtures, examples and migration guidance; required language bindings share defined semantics. | Published foundations; stateful source/view/candidate contracts and adapter integration remain in progress. |
| eosrio/hyperion-plugins | Official catalog metadata and declared compatibility with tested release pins. | Reuse the existing catalog repository. |
| eosrio/hyperion-atomicassets-plugin | One operator package for AtomicAssets and AtomicMarket indexing and API parity, with modular internals. | Original PostgreSQL oracle and pure formatters published; portable lifecycle and full API parity remain open. |
| eosrio/hyperion-wire-plugin | One operator package for Wire protocol support, including coordinated Node/Rust components. | Provider and codec foundations published; selected testnet and full-worker qualification remain open. |
| igorls/wormdb | Database engine capabilities and engine-specific correctness/performance fixes. | Existing; file engine work here when demonstrated necessary. |
| eosrio/hyperion-docs | Operator-facing upgrade, installation, configuration and recovery documentation. | Existing; publish operator guidance as behavior becomes qualified. |

Backend adapters remain separately versioned extension packages. The
[component foundation](../architecture/hyperion-4.5-component-homes.md) assigns their initial source
homes and package names, alongside the existing runtime and new SDK/plugin packages. A package
does not automatically need another repository or a new service/worker rewrite.

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
| M4: Release qualification and operator distribution | A retained-history upgrade, pinned component distribution, recovery exercises, explorer compatibility and operator workflows pass together for the supported matrix. Backend qualification decision and known limits are published. | All mandatory outcomes; qualified MongoDB is sufficient if WormDB is deferred. |

Existing history compatibility and reliability checks run from the beginning. They are not
postponed until M4. Wire and AtomicAssets work should expose missing generic capabilities early,
instead of waiting for a speculative complete SDK.

### Current delivery checkpoint: 2026-10-27

This is a progress snapshot of the linked work, not completion of M1–M4.

| Workstream | Verified result | Remaining gate |
|---|---|---|
| Stateful SDK | [Proposal #33](https://github.com/eosrio/hyperion-sdk/pull/33) and [internal value foundation #34](https://github.com/eosrio/hyperion-sdk/pull/34) merged. The value slice passed 100 tests on each native Node 24/26 runtime, packed consumers, independent Standards/Spec review and product CI. | [SDK #32](https://github.com/eosrio/hyperion-sdk/issues/32) still needs the source/view/candidate/reducer and adapter conformance seams. The proposal remains proposed; no new family is activated by the internal value helper. |
| AtomicAssets and AtomicMarket | [Original PostgreSQL oracle #13](https://github.com/eosrio/hyperion-atomicassets-plugin/pull/13) and [pure sale formatter #15](https://github.com/eosrio/hyperion-atomicassets-plugin/pull/15) merged. Both native Node 24/26 original PostgreSQL 16.15 runs passed seven SQL/view/HTTP stages, expected rejection controls and owned cleanup. | [Asset lifecycle #10](https://github.com/eosrio/hyperion-atomicassets-plugin/issues/10) needs actual portable MongoDB integration and coherent reads. [Full 2.7.0 parity #1](https://github.com/eosrio/hyperion-atomicassets-plugin/issues/1), marketplace state and remaining APIs stay open. |
| Wire worker/MongoDB candidate | Complete source review and CI pass for [History API #361](https://github.com/eosrio/hyperion-history-api/pull/361). Its latest plain Node 24 control passed 34 cursor waits, then failed target 1037 with completed recovery polls at 1036. Original cleanup, host guards and independent complete custody verification passed. | Keep #361 draft. Fault 1244, final 1480, current full replay and the remaining recovery/launch matrix are unqualified. Source ACKs and green component checks do not close [#191](https://github.com/eosrio/hyperion-history-api/issues/191) or [#277](https://github.com/eosrio/hyperion-history-api/issues/277). |

The [current profiling checkpoint](../performance/hyperion-4.5-profiling.md#native-wire-and-domain-foundation-checkpoint-2026-10-27)
records the two separate failed Wire observations and their evidence boundaries.
The next Wire diagnostic needs a faithful restartable backlog fixture; a raw block corpus alone
does not preserve store state, queued bodies and in-flight ownership. The original deadline and
correctness gates remain intact. Experimental capacity stays off and requires an explicit per-chain
qualification budget with no derived default.

Retained-history upgrades, Bun runtime support, stream-client integration, final Wire launch pins,
WormDB qualification, explorer delivery and operator distribution remain separate release gates.
These component merges do not close them.

## Parallel workstreams and release gates

- **Component profiling and runtime qualification:** follow the
  [profiling program](../performance/hyperion-4.5-profiling.md) across every shipping component.
  Establish guarded, ABI-correct fixtures and correctness oracles, then collect CPU/native,
  allocation/GC, queue/wait, storage and p50/p95/p99 profiles under live, bulk and mixed workloads.
  Evaluate the current Bun 1.4.4 candidate against the Node controls with explicit runtime/transport qualification.
  Review Elasticsearch release notes and qualify server/client changes without requiring a
  full Hyperion history reindex. Historical reports and faster typechecking are not runtime evidence.
- **Upgrade and reliability:** preserve usable existing Elasticsearch history and continued history
  indexing without a full reindex; initialize new state separately. Exercise failure/retry,
  reconnect, backpressure and live-priority behavior using the existing worker infrastructure.
- **Prospective capacity admission:** [History API #277](https://github.com/eosrio/hyperion-history-api/issues/277),
  coordinated through [release issue #24](https://github.com/eosrio/hyperion-tools/issues/24), must
  qualify the experimental count-and-byte path with an explicit per-chain aggregate byte budget
  and no derived default. Existing installations retain current behavior until explicit enablement.
  Numeric budgets, operator-facing units, configuration syntax and activation remain undecided.
  Preserve existing queue/resume, prefetch, source-window, ancestry and deadline constraints,
  [infrastructure backpressure and contract isolation](../adr/0022-retain-infrastructure-backpressure.md),
  and the original replay/recovery/shutdown gates. This policy decision does not complete those gates.
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
  and recovery. Investigation revisions are not accepted launch pins. The new testnet's synced
  SHiP node is available on Hyperion-3; final live qualification still requires the selected
  contract/build/deployment receipts and the integrated Node/Rust recovery exercises.
- **WormDB:** assess required capabilities early and file demonstrated engine gaps in WormDB.
  Qualify the unchanged definitions and plugin queries against the same applicable conformance,
  recovery and parity requirements. If the effort is too high, record an explicit MongoDB-only
  release decision; permanent backend support remains the architecture direction.
- **Sequence identity and client migration:** [History API #219](https://github.com/eosrio/hyperion-history-api/issues/219)
  implements exact decimal-string uint64 sequence fields on all chains, including Antelope, under
  [ADR 0053](../adr/0053-use-lossless-sequence-strings-on-all-chains.md). Qualify public aliases,
  query/filter/sort/page behavior, cache/stream/archive paths and retained-history exact coverage;
  document the intentional 4.5 response-type change without requiring a full history reindex.
- **Stream client/server compatibility:** complete [History API #231](https://github.com/eosrio/hyperion-history-api/issues/231)
  across real history/live filters, reconnect cleanup, cancellation and request-scoped terminal
  failures. Qualify exact sequence strings and retryable errors through the actual client/server
  route, with source-count/identity, transport-drop, fork and LIB checks. Client-only fixtures
  do not satisfy server or Elasticsearch completeness acceptance.
- **Explorer modernization and compatibility:** upgrade the existing frontend and qualify its
  routes, SSR/proxy deployment, live streaming and browser behavior against the 4.5 contracts.
  Preserve exact sequence identities and distinguish unavailable projections from empty results.
  Frontend work starts independently; the [explorer delivery slices](#explorer-delivery-slices)
  join backend integration as contracts become available and complete release acceptance in M4.
- **Distribution and operator lifecycle:** independent SDK/plugin/adapter releases, tested versions
  pinned for Hyperion, bundled first-party plugins disabled by default, catalog compatibility checks,
  one-package Wire installation and controlled executable updates. Qualify backend cutover after
  destination initialization/catch-up/validation; a brief state API pause is permitted.

Do not schedule transport replacement, an untrusted-code sandbox, a wholesale worker-pool rewrite
or zero-downtime backend cutover as prerequisites for 4.5. Revisit only with a concrete requirement
or measurement and a separate scope decision.

## Stream client compatibility gate

- [ ] Complete [History API #231](https://github.com/eosrio/hyperion-history-api/issues/231)
  before M4 acceptance, coordinated with [qualification matrix #20](https://github.com/eosrio/hyperion-tools/issues/20)
  and [release issue #24](https://github.com/eosrio/hyperion-tools/issues/24).

Use #231 as the implementation and acceptance source; do not duplicate its backlog. Server
fixes can proceed alongside other backend work. Package, browser and loopback client tests
are supporting evidence; closing this gate requires the actual server/client integration
against a known Antelope dataset and its history/live recovery checks.

## Explorer delivery slices

Implementation belongs in [hyperion-explorer](https://github.com/eosrio/hyperion-explorer), with
scoped issues and PRs linked to [release coordination #24](https://github.com/eosrio/hyperion-tools/issues/24).
This extends the roadmap without renumbering the published initial issue batch.

1. **Frontend foundation (starts alongside M0):** upgrade Angular/SSR, Material, chart/icon/animation
   libraries and other dependencies to mutually supported versions. Preserve existing explorer
   workflows and deployment themes while improving responsive layout, keyboard access, contrast,
   loading/error/empty states and bundle loading. Establish reproducible installs, unit/browser
   tests and production builds. Document the frontend's framework-specific TypeScript and Node
   constraints; the backend SDK/toolchain policy does not override Angular compiler compatibility.
2. **4.5 client integration (alongside M1-M3):** use the selected deployment for all API, oracle and
   stream requests. Preserve canonical decimal-string sequence identifiers through rendering,
   filters, ordering, pagination and streaming, including values above JavaScript's safe integer
   range, under [ADR 0053](../adr/0053-use-lossless-sequence-strings-on-all-chains.md) and
   [History API #219](https://github.com/eosrio/hyperion-history-api/issues/219). Handle scoped
   retryable unavailability and explicit recovery without presenting failed requests as empty
   results or disabling unrelated history. Consume completeness/last-valid-state headers when
   their shared contract is defined; history indexing progress does not establish state readiness.
3. **Wire testnet preview (planned):** serve a pinned new explorer build with Hyperion 4.5 as the
   main page at [testnet.wire.eosrio.io](https://testnet.wire.eosrio.io/), tracked in
   [Explorer #4](https://github.com/eosrio/hyperion-explorer/issues/4). Use the intended Wire chain
   and API configuration, support root-page assets and deep links, and preserve API, documentation
   and streaming routes. Verify real Wire browser/SSR workflows, served build identities and rollback
   before activation. Schedule the preview after a usable pinned Wire API and applicable client
   integration checks; preview success does not complete Wire or 4.5 release qualification.
4. **Release acceptance (M4):** test the built explorer behind the real `/explorer` API proxy with
   pinned API and stream-client artifacts. Cover accounts, blocks, transactions, public keys,
   contract tables, filters/pagination, and enabled top-holder/oracle features. Exercise live
   streaming reconnect and finality/fork recovery against representative supported deployments,
   including each offered state backend where the workflow depends on state. Qualify SSR/hydration,
   host configuration, deployment themes and the supported desktop/mobile browser matrix; publish
   the tested versions, upgrade instructions and remaining limits.

The 2026-10-20 local checkpoint has an initial Angular 22/TypeScript 6/Vitest 5 upgrade and frontend
improvements, with passing production build/typecheck, 49 unit tests and 12 desktop/mobile Chromium
fixture scenarios. Those changes are local and uncommitted at this checkpoint; published CI,
lossless sequence migration, live 4.5 integration and broader browser/backend qualification remain
pending. Fixture success does not complete explorer release acceptance.

## First issue batch

The [published initial issue batch](hyperion-4.5-initial-issues.md) defines ten bounded starting outcomes:

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


## Publication record — 2026-10-17

Release coordination: [hyperion-tools #24](https://github.com/eosrio/hyperion-tools/issues/24).
The ten issues below are native sub-issues of that release issue; genuine blockers are also
registered as native GitHub dependencies.

| Planning ID | Published issue | Milestone | Current prerequisites |
|---|---|---|---|
| P01 | [eosrio/hyperion-tools #19](https://github.com/eosrio/hyperion-tools/issues/19) | M0 | None |
| P02 | [eosrio/hyperion-tools #20](https://github.com/eosrio/hyperion-tools/issues/20) | M0 | None |
| P03 | [eosrio/hyperion-history-api #189](https://github.com/eosrio/hyperion-history-api/issues/189) | M1 | None |
| P04 | [eosrio/hyperion-history-api #190](https://github.com/eosrio/hyperion-history-api/issues/190) | M0 | [P02](https://github.com/eosrio/hyperion-tools/issues/20) |
| P05 | [eosrio/hyperion-sdk #1](https://github.com/eosrio/hyperion-sdk/issues/1) | M1 | [P01](https://github.com/eosrio/hyperion-tools/issues/19) |
| P06 | [eosrio/hyperion-history-api #191](https://github.com/eosrio/hyperion-history-api/issues/191) | M1 | [P05](https://github.com/eosrio/hyperion-sdk/issues/1) |
| P07 | [eosrio/hyperion-tools #21](https://github.com/eosrio/hyperion-tools/issues/21) | M2 | [P05](https://github.com/eosrio/hyperion-sdk/issues/1) |
| P08 | [eosrio/hyperion-tools #22](https://github.com/eosrio/hyperion-tools/issues/22) | M3 | None |
| P09 | [eosrio/hyperion-tools #23](https://github.com/eosrio/hyperion-tools/issues/23) | M4 | None |
| P10 | [eosrio/hyperion-atomicassets-plugin #1](https://github.com/eosrio/hyperion-atomicassets-plugin/issues/1) | M3 | None |

The repository-creation prerequisite for P10 is fulfilled. P01 remains open for adapter source
homes, SDK migration policy and the catalog compatibility format/consumer audit; repository
creation alone does not complete that issue. P03 is marked ready-for-agent. Setup and investigation
work can start independently without being mislabeled as implementation-ready.

Repository bootstrap is published on main in [hyperion-sdk](https://github.com/eosrio/hyperion-sdk),
[hyperion-atomicassets-plugin](https://github.com/eosrio/hyperion-atomicassets-plugin) and
[hyperion-wire-plugin](https://github.com/eosrio/hyperion-wire-plugin). Each has MIT licensing,
contribution and agent guidance, a scoped task template, and passing initial scaffold CI.

The architecture/plan baseline was merged through [tools PR #18](https://github.com/eosrio/hyperion-tools/pull/18).
Catalog preparation is [catalog PR #1](https://github.com/eosrio/hyperion-plugins/pull/1), preserving
legacy entries and adding guidance plus structural CI. The compatibility schema migration remains
P01 work. Runtime implementation and release qualification remain pending.

## Replay and recovery checkpoint: 2026-10-19

The [profiling program](../performance/hyperion-4.5-profiling.md#guarded-replay-and-reader-recovery-checkpoint-2026-10-19)
records the first native Linux results for parallel API #195/#208 work. The ABI
deployment decoder correction is [API #209](https://github.com/eosrio/hyperion-history-api/pull/209).
Range/generation confirmation tracking and retained reader assignments are
[API #210](https://github.com/eosrio/hyperion-history-api/pull/210), with seven
passing actual broker/worker cases and three baseline defect reproductions.

The finite known-genesis contract replay in
[API #211](https://github.com/eosrio/hyperion-history-api/pull/211) passes exact input,
ABI and two-copy output checks. This is a correctness foundation for profiling; it does not complete
the release gates or qualify a runtime/server upgrade. API #195 remains open for
persisted checkpoint/restart evidence. API #208 remains open for integrated live
fork/restart recovery and late old-generation broker callbacks. API #196 owns the
subsequent reader-to-ack profiles; API #197 transport work is deferred by the operator.

## Native Wire profiling checkpoint: 2026-10-24

The [current profiling ledger](../performance/hyperion-4.5-profiling.md#current-native-wire-capture-checkpoint-2026-10-24)
now links the audited 14-process native Wire capture in
[History API #298](https://github.com/eosrio/hyperion-history-api/pull/298).
The frozen Node 24 run passed holds 1/33/122/256/397 but **failed** the unchanged
685 progress wait. Profile completeness, preserved evidence and clean teardown do not
complete the native worker/Mongo path, live fork/restart, #277 proof-capacity or Wire release gates.

The measured stacks distinguish qualification-observer sync calls from production work.
That CPU-only capture left measurements in #277/#248/#196: observer write/sync latency, actual
per-open ancestry GET/bytes/waits and queue residence. Its backend/host I/O, dense-input
retention and paired overhead comparisons are explicitly unmeasured. Preserve the current
source/oracles, 256-block ancestry, queue policy and deadlines before selecting an optimization.

## Native Wire I/O checkpoint: 2026-10-24

The [subsequent profiling ledger](../performance/hyperion-4.5-profiling.md#native-wire-prooflogger-measurement-checkpoint-2026-10-24)
records [History API #302](https://github.com/eosrio/hyperion-history-api/pull/302)
and the independently verified [measurements](https://github.com/eosrio/hyperion-history-api/blob/67b544e4f720606e97171dbad999a7e8a92b64b4/docs/performance/native-wire-proof-io-20261024.md).
The new Node 24 run again failed the original 685 wait at 661, with complete
14-process CPU and 6,569-proof I/O accounting and successful original cleanup.
Actual proof-open HTTP counts/body sizes and logger sync latency are now measured;
source-modeled endpoint counts, exclusive waits, incremental overhead and full
qualification remain distinct. The old frozen capture is preserved.

The mandatory correctness frontier remains completing #288's same-attempt matrix,
then #277 prospective count/byte coordination and the complete original 1..1480
replay/canonical recovery gates. Controlled read/logger experiments and #248's
dense retained-memory work remain open. No optimization, workload/budget/deadline
change, runtime ranking or release-readiness conclusion is selected by this checkpoint.

## Native Wire broker recovery checkpoint: 2026-10-25

[History API #320](https://github.com/eosrio/hyperion-history-api/pull/320) merged into
still-draft [#289](https://github.com/eosrio/hyperion-history-api/pull/289), completing
the scoped [#319](https://github.com/eosrio/hyperion-history-api/issues/319) owned-broker
handoff case. The actual Linux Node 24 run verifies the held/fenced original receipt,
the replacement's own fresh write/end/retained association, exactly both original
block-33 publishers, separate broker counter windows and resumed block-34 state,
native and API checks. Broker settlement remains unknown.

[The frozen evidence and gate record](https://github.com/eosrio/hyperion-history-api/issues/319#issuecomment-6399124139)
has two independent custody audits covering 19,155 payloads and all source, emission,
component, input and oracle pins. Both unchanged full workloads **failed** at the
original 685 barrier. The fault's master exit 1 and false software-shutdown assertion
are preserved; physical cleanup and incumbent preservation were independently verified.

The remaining [#288 recovery matrix](https://github.com/eosrio/hyperion-history-api/issues/288)
includes generic/reversible inputs and other combined storage/live-fork cases.
[#277](https://github.com/eosrio/hyperion-history-api/issues/277) still requires prospective
count/byte coordination and the complete original 1..1480 replay on Node 24/26.
The parent stays draft; source/oracles, 256-block ancestry, budgets, durability,
defaults and deadlines are unchanged. This checkpoint grants no capacity or full
Wire/release qualification.

## Publication and evidence

The [Wire implementation delivery plan](hyperion-4.5-wire-implementation.md) records the
2026-10-20 native-genesis candidates, actual block-1–4 source evidence, owning issues and remaining
launch decisions. Reviewed [SDK #10](https://github.com/eosrio/hyperion-sdk/pull/10),
[Wire #14](https://github.com/eosrio/hyperion-wire-plugin/pull/14) and
[History API #241](https://github.com/eosrio/hyperion-history-api/pull/241) supply the portable
contract, source-semantic verifier and durable host admission; all three PRs are merged.
Their first actual-source regression used in-memory broker/storage seams.

The 2026-10-21 [production integration candidate, History API #242](https://github.com/eosrio/hyperion-history-api/pull/242),
for [#191](https://github.com/eosrio/hyperion-history-api/issues/191), now passes the existing native
Node 22/24 workers, RabbitMQ, actual MongoDB adapter and public HTTP API against original Wire
testnet blocks 1–1480 on Hyperion-3. Independent physical-row/official-clio oracles verify
insert/update/delete results. Actual Mongo write faults, exact-source retry, scoped query
unavailability, continuing block history, source/broker acknowledgments and clean shutdown are
retained in the [checksummed evidence](https://github.com/eosrio/hyperion-history-api/tree/codex/portable-state-production/docs/performance/evidence/portable-state-worker-20261021).
The prerequisite [SDK #13](https://github.com/eosrio/hyperion-sdk/pull/13) adds a non-serving durable
recovery view. These two PRs remain pending merge at this checkpoint; the M1 integration is a
verified candidate, not a completed release-branch milestone.

The finite replay uses native workers inside an owned six-CPU/12-GiB Linux container with isolated
services. Per-process CPU/RSS observations are profiling leads, not bare-metal throughput, Bun,
memory-recovery or complete Wire qualification. Five original one-copy Elasticsearch startup
acknowledgments remain explicit in each run. The original host services are verified unchanged,
the Wire head advances, and owned test services/container have zero residue.

M2 auto-map, live mapping reload and actual process-crash/fork recovery remain required.
M3 still requires the applicable Rust paths, selected build/deployment inputs and full testnet
qualification. [Wire #15](https://github.com/eosrio/hyperion-wire-plugin/issues/15) records actual
`sysio::onblock` decoding discrepancies; a separate rs_abieos compatibility effort is in progress
and must be evaluated against that evidence. The stream-client integration gate
[#231](https://github.com/eosrio/hyperion-history-api/issues/231), AtomicAssets/AtomicMarket parity,
WormDB qualification and the remaining release matrix remain open.

The operator approved the repository names and first batch on 2026-10-17. The architecture baseline,
release issue and initial component issues are published with real cross-repository references
and blocking links. Investigation/setup issues must
not be labeled implementation-ready merely because they are on the roadmap.

All implementation issues cite the published architecture baseline, state their delivered behavior,
and define acceptance evidence and genuine blockers. Milestones are complete only when their
integration and operator outcomes pass; a source inspection, unit test or benchmark alone is not
release qualification. Estimate dates after the first slices establish effort and available capacity.
