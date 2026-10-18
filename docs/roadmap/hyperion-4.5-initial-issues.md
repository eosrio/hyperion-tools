# Hyperion 4.5: approved initial issue batch

Status: Approved and published on 2026-10-17. P01-P10 are stable planning identifiers.
[Release coordination issue #24](https://github.com/eosrio/hyperion-tools/issues/24) and the
[roadmap publication record](hyperion-4.5.md#publication-record--2026-10-17) map the live frontier.

Source: [release roadmap](hyperion-4.5.md) and [architecture definition](../architecture/hyperion-4.5.md).
The detailed decisions remain binding. This first batch opens the delivery frontier; it is not
the entire 4.5 backlog. Later milestone work needs its own bounded issues.

## P01: Establish the 4.5 SDK and plugin repository homes

**Published issue:** [eosrio/hyperion-tools #19](https://github.com/eosrio/hyperion-tools/issues/19).

**Lead repository:** eosrio/hyperion-tools (setup coordination).
**Type:** Repository setup and migration planning.
**Blocked by:** None; repository names/ownership approved on 2026-10-17.

**What it delivers:** Maintainers and agents have clear source, release and compatibility homes for
the SDK and first-party extensions, without replacing existing plugin infrastructure silently.

Acceptance criteria:

- [x] Create the reviewed SDK, AtomicAssets and Wire repositories with explicit ownership,
  contribution/license policy and minimal appropriate CI; avoid publishing empty stable SDK releases.
- [ ] Record package names and source homes for the runtime pieces and separately versioned MongoDB
  and WormDB adapter extensions. Package boundaries do not require separate processes.
- [ ] Record how current hyperion-plugin-core consumers migrate or remain supported; identify
  existing first-party plugins that need migration qualification.
- [ ] Add a documented compatibility metadata format and validation fixture to the existing
  hyperion-plugins catalog, with an explicit policy for legacy entries and checked consumers.
- [ ] Link the architecture baseline and component ownership from each relevant repository.
  No plugin execution or automatic code update is triggered by catalog metadata.

## P02: Define the 4.5 release qualification matrix

**Published issue:** [eosrio/hyperion-tools #20](https://github.com/eosrio/hyperion-tools/issues/20).

**Lead repository:** eosrio/hyperion-tools.
**Type:** Bounded release planning.
**Blocked by:** None; can proceed alongside repository setup.

**What it delivers:** A finite set of supported upgrade sources, API families, chains and component
combinations against which the release can be accepted.

Acceptance criteria:

- [ ] Select supported source Hyperion versions and Elasticsearch configurations and identify a
  representative retained-history fixture for the first upgrade test.
- [ ] Record mandatory AtomicAssets/AtomicMarket parity, Wire testnet support, default reversible
  mode and optional per-chain irreversible-only mode without weakening accepted requirements.
- [ ] Identify the existing state/API surfaces that need preservation or explicit migration decisions,
  including current public Mongo-shaped table-query filters; do not silently redefine their behavior.
- [ ] Separate MongoDB launch requirements from conditional WormDB qualification and record how a
  backend's exact supported engine/adapter versions will be pinned.
- [ ] Resolve remaining scope questions at release level; leave component implementation choices
  to their own issues.

## P03: Keep failed MongoDB writes retryable

**Published issue:** [eosrio/hyperion-history-api #189](https://github.com/eosrio/hyperion-history-api/issues/189).

**Lead repository:** eosrio/hyperion-history-api.
**Type:** Correctness fix.
**Blocked by:** None.

**What it delivers:** A rejected MongoDB write cannot be counted as successfully indexed or cause
its delivered work to be acknowledged as successful.

Acceptance criteria:

- [ ] Cover both existing named MongoDB routes and dynamic contract-state routes.
- [ ] Propagate write failures to the worker's failure/retry path; acknowledge successful work only
  after the required writes succeed. Review actual consumer/requeue semantics.
- [ ] Inject a rejected write and demonstrate that its work remains recoverable, including partial
  success within a batch and a successful retry.
- [ ] Demonstrate that success statistics and completion callbacks do not report failed writes as
  successful, while unrelated processing continues.

Source inspection found completion callbacks in finally paths feeding the worker acknowledgment.
This needs executable regression evidence. Related reliability tracking: history-api #187; this is
a distinct write-acknowledgment defect.

## P04: Prove a 4.5 upgrade preserves existing history

**Published issue:** [eosrio/hyperion-history-api #190](https://github.com/eosrio/hyperion-history-api/issues/190).

**Lead repository:** eosrio/hyperion-history-api, with tooling support as needed.
**Type:** Upgrade integration slice.
**Blocked by:** P02 selects the first supported source baseline and fixture.

**What it delivers:** An operator starts the candidate 4.5 integration over an existing history
dataset, can query retained history and continue indexing, and can initialize state separately.

Acceptance criteria:

- [ ] Use history produced by the selected supported source version and retain representative query
  results as the compatibility oracle.
- [ ] Show retained history remains queryable and new history continues without a full history
  reindex. Record any supported metadata/configuration migration explicitly.
- [ ] State initialization is independently resumable and does not require rebuilding all history;
  unavailable new state does not make unrelated existing history queries unavailable.
- [ ] Make the fixture and focused check reusable as later SDK/plugin changes land. Extend coverage
  to the full supported upgrade matrix before release.

This establishes a repeatable guard, not final qualification of an otherwise unfinished 4.5 release.

## P05: Round-trip one contract through the SDK and MongoDB adapter

**Published issue:** [eosrio/hyperion-sdk #1](https://github.com/eosrio/hyperion-sdk/issues/1).

**Lead repository:** eosrio/hyperion-sdk; linked adapter/host changes in their assigned homes.
**Type:** First portable vertical slice.
**Blocked by:** P01 establishes the SDK and adapter source homes.

**What it delivers:** One declarative contract definition processes representative events into a real
MongoDB projection and returns correct results through a backend-neutral logical query.

Acceptance criteria:

- [ ] Implement only the public contracts needed for this slice, with explicit capability validation.
  Use the same source definition and query without embedded native MongoDB operations.
- [ ] Exercise insert, update and delete, stable record identity and ordered processing.
- [ ] Demonstrate duplicate delivery and a write failure/retry without duplicated effects or progress
  advancing past unapplied work. Effects and recoverable progress satisfy the accepted durability contract.
- [ ] Publish a reusable example and real-database conformance fixture. Declare unimplemented
  capabilities explicitly; this slice does not claim complete AtomicAssets query support.
- [ ] Specify how host/runtime and adapter responsibilities meet across the package boundary;
  do not introduce contract-specific logic into Hyperion core.

## P06: Index and serve the first portable contract in Hyperion

**Published issue:** [eosrio/hyperion-history-api #191](https://github.com/eosrio/hyperion-history-api/issues/191).

**Lead repository:** eosrio/hyperion-history-api.
**Type:** Worker-to-API integration slice.
**Blocked by:** P05 supplies the usable SDK/adapter path.

**What it delivers:** Enabling one allowed contract through chain configuration uses existing workers
and RabbitMQ to maintain state and serves it through the Hyperion API host.

Acceptance criteria:

- [ ] Register the definition and selected per-chain backend through existing operator/worker flows.
  Reject missing required capabilities before projection activation.
- [ ] Feed real decoded events through the live worker path and verify API-visible insert/update/delete
  results using the shared query interface and actual MongoDB adapter.
- [ ] Retry failed delivered work safely; exercise the actual acknowledgment path, including the
  P03 regression when that existing MongoDB route is reused.
- [ ] A contract processing failure leaves affected state explicitly unavailable/incomplete while
  unrelated indexing and existing history queries continue.
- [ ] Establish a reusable integration fixture for later lifecycle/fork tests. Full auto-map and
  staged live mapping reload remain required follow-up slices under M2.

## P07: Prove projection rollback survives restart

**Published issue:** [eosrio/hyperion-tools #21](https://github.com/eosrio/hyperion-tools/issues/21).

**Lead repository:** eosrio/hyperion-tools; adapter changes in the P01-assigned source home.
**Type:** Runtime/adapter recovery slice.
**Blocked by:** P05 supplies the first durable projection fixture.

**What it delivers:** Runtime-coordinated rollback restores a projection to its canonical ancestor
and applies a replacement branch correctly, including after interruption.

Acceptance criteria:

- [ ] Exercise orphaned inserts, updates and deletes, then a replacement branch and duplicate delivery.
- [ ] Interrupt and restart around durable apply, checkpoint and rollback boundaries; retained
  progress and stored state must agree after recovery.
- [ ] Use branch identity, not block number alone, and verify the resulting canonical view.
- [ ] Demonstrate bounded local recovery and an older-input gap that leaves the affected projection
  incomplete rather than silently advancing it.
- [ ] Document supported recovery coverage and provide a repeatable real-MongoDB fixture.

This tests the runtime/adapter boundary. Query pausing, last-valid response policy, irreversible-only
behavior and live SHiP fork/reconnect qualification require subsequent host integration slices.

## P08: Define a reproducible Wire testnet qualification profile

**Published issue:** [eosrio/hyperion-tools #22](https://github.com/eosrio/hyperion-tools/issues/22).

**Lead repository:** eosrio/hyperion-tools; hand off extension implementation to the Wire repo.
**Type:** Protocol qualification preparation.
**Blocked by:** None for profile definition; repository creation gates later extension PRs.

**What it delivers:** A concrete launch/fixture profile makes the Wire extension implementable and
testable before the new testnet's final live acceptance.

Acceptance criteria:

- [ ] Select or explicitly leave pending the exact node build, contract artifacts, genesis parameters
  and deployment sequence; investigation commits are not silently adopted as release pins.
- [ ] Define representative fixtures for timestamp interpretation, ABI/value codecs, primary and
  secondary KV records, complete keys and ABI deployment changes.
- [ ] Identify required Node/Rust decode paths and expected equivalent results.
- [ ] Define fork/reconnect and live/bulk exercises and who provisions the new testnet with Wire's
  contracts. Record the live testnet as a dependency of final qualification.
- [ ] Produce bounded implementation issues from demonstrated gaps; do not port the pseudo-fork wholesale.

## P09: Assess WormDB against the 4.5 backend contract

**Published issue:** [eosrio/hyperion-tools #23](https://github.com/eosrio/hyperion-tools/issues/23).

**Lead repository:** eosrio/hyperion-tools; demonstrated engine gaps become linked igorls/wormdb issues.
**Type:** Capability investigation.
**Blocked by:** None for assessment; final shared conformance uses the contracts established by P05
and subsequent domain/recovery slices.

**What it delivers:** A source- and test-backed assessment identifies whether WormDB is practical
for 4.5 and which capabilities need adapter or engine work.

Acceptance criteria:

- [ ] Classify required capabilities as supported with evidence, missing, or unverified; distinguish
  adapter gaps from engine gaps. Do not infer current production guarantees from a benchmark.
- [ ] Probe high-risk durability, replay, rollback, read consistency and pagination behavior against
  selected engine revisions and declared query needs.
- [ ] Include AtomicAssets/AtomicMarket filtering, sorting, counts and pagination requirements as
  the pinned parity inventory becomes available.
- [ ] Link bounded engine/adapter follow-ups with estimated effort and evidence; support an explicit
  qualify-for-4.5 or MongoDB-only recommendation.
- [ ] Preserve the permanent backend-neutral definitions/query contract in either recommendation.
  This assessment alone does not qualify WormDB or defer it automatically.

## P10: Establish the AtomicAssets and AtomicMarket parity inventory

**Published issue:** [eosrio/hyperion-atomicassets-plugin #1](https://github.com/eosrio/hyperion-atomicassets-plugin/issues/1).

**Lead repository:** eosrio/hyperion-atomicassets-plugin.
**Type:** Parity baseline and reusable test harness.
**Blocked by:** None. The repository-creation prerequisite from P01 is satisfied; remaining
SDK/catalog setup does not block this inventory.

**What it delivers:** A pinned API inventory and representative reference dataset allow endpoint
families to be implemented and judged against the required 2.7.0 behavior.

Acceptance criteria:

- [ ] Pin atomicassets/atomicassets-api 2.7.0 to commit
  0aabc47dcf8bd0804d4dcecb336c90fc02a9c89e in the reference harness.
- [ ] Inventory the complete AtomicAssets and AtomicMarket API surface, including query semantics,
  response/error behavior and pagination, and make unimplemented coverage visible.
- [ ] Capture a representative reference fixture and run at least one reference request end to end;
  distinguish deterministic comparisons from environment-dependent values.
- [ ] Record backend-neutral query/projection capabilities and historical input coverage required
  by the inventory. Include market relationships and asset lifecycle cases.
- [ ] Propose bounded endpoint-family slices, each with its needed indexing path and acceptance
  evidence. A passing initial request is not a parity completion claim.

## Existing issues to include in the release frontier

Reuse [history-api #188](https://github.com/eosrio/hyperion-history-api/issues/188) for the fork
cleanup report and [history-api #187](https://github.com/eosrio/hyperion-history-api/issues/187)
for its resilience/observability reports. Reproduce against the selected baseline and link focused
fixes as needed. Do not duplicate, close or rewrite those parent issues as part of publishing this batch.

Native blocking links and release sub-issue relationships are published using real issue identifiers. P01/P02/P08/P09/P10
contain setup or investigation outcomes; label them accordingly rather than marking the entire
batch implementation-ready.
