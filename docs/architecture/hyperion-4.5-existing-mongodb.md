# Hyperion 4.5: existing MongoDB contract-state baseline

Inspected on 2026-10-16 in the local `hyperion-history-api` working tree, based on HEAD
`79af10986bc0e2aa84cce3788aea202d23547818`. The checkout contains unrelated uncommitted changes,
including configuration-schema changes; this records the inspected working tree, not a claim that
all behavior is present in that commit alone or in a deployed release. No implementation files were
changed during this inspection.

This supports the [architecture definition](hyperion-4.5.md). It covers the contract-state discussion,
not an exhaustive inventory of Hyperion's Mongo-backed domains. All source paths below are relative
to `hyperion-history-api`.

## Existing mechanisms

| Area | Observed mechanism | Source entry points |
|---|---|---|
| Operator configuration | `features.contract_state` enables state indexing and selects contract accounts and individual tables. Each table has `auto_index` and explicit `indices`. | `src/interfaces/hyperionConfig.ts`, `features.contract_state` and its validation schema |
| Configuration CLI | Adding/updating contract tables writes the chain configuration and requests a contract-config reload. Both single-table and multiple-table commands exist. | `src/cli/hyp-config.ts`, `addOrUpdateContractConfig`, `add-single`, `add-multiple`, `reloadConfig` |
| Automatic/manual physical indexes | The master creates metadata indexes, then either derives a single-field index for each field in the table's ABI struct/base structs or creates configured indexes, including text indexes. ABI-derived setup fetches the current ABI from chain RPC. | `src/indexer/modules/master.ts`, `parseContractStateConfig` |
| Runtime contract-config reload | The controller calls the master's contract reload. It reloads configuration, prepares indexes for that contract, notifies deserializer workers, and waits for their reload acknowledgments. | `src/cli/controller-client/controller.client.ts`, `reloadContractStateConfig`; `src/indexer/modules/controller.ts`, `reload_contract_config`; `src/indexer/modules/master.ts`, `reloadContractConfig`; `src/indexer/workers/deserializer.ts`, `reload_config` |
| Live contract-state updates | Configured contract/table rows are sent to dynamic indexing queues. The Mongo route groups operations by contract/table collection and upserts or deletes using scope and primary key. Documents include decoded row fields and block/payer metadata. | `src/indexer/workers/deserializer.ts`, `pushToDynamicTableQueue`; `src/indexer/helpers/mongo-routes.ts`, `addDynamicRoutes` |
| Initial/current-state synchronization | The synchronizer enumerates configured tables and scopes through chain RPC, fetches rows, creates indexes, and writes state into MongoDB. The control command wraps this in pause/resume of the `dynamic-table` indexing type. | `src/cli/sync-modules/sync-contract-state.ts`, `ContractStateSynchronizer`; `src/cli/hyp-control.ts`, `syncContractState`, `syncWithPauseResume` |
| State table-query API | `/v2/state/get_table_rows` queries the contract/table collection with metadata and JSON field filters, sorting, skip/limit pagination, and matching-row counts. | `src/api/routes/v2-state/get_table_rows/get_table_rows.ts`, `getTableRows` |
| ABI history and decode updates | Account ABI deltas are indexed with their contract/block and ABI content; live ABI changes update the decoder and clear that contract's dynamic blacklist. Historical lookup searches ABI records at/before a requested block. | `src/indexer/workers/deserializer.ts`, `deltaStructHandlers.account`, `fetchAbiHexAtBlockElastic`, `verifyLocalType` |

These are implemented code paths. This inspection did not run a live reload, sync, query conformance
suite, or production acceptance test, and it does not certify every error/recovery branch.

There is also relevant existing work in `hyperion-tools`: `crates/snapshot-load/src/atomicassets.rs`
and `atomicmarket.rs` map snapshot state into domain-shaped Mongo documents, including AtomicData
decoding through a schema-format registry. `crates/snapshot-load/src/mongo.rs` provides the bulk
Mongo sink and domain indexes, including attribute wildcard indexes. This is a snapshot bootstrap
implementation; it does not by itself establish equivalent live updates or API endpoint parity.
These paths were inspected at local `hyperion-tools` HEAD
`b6b774c8b3f3b074f7cc4a172631f41a6639ca5f`.

## How this changes the 4.5 work definition

- **Retain and build on existing Mongo functionality.** Contract/table configuration, ABI-driven
  indexing, live reload, state writes, synchronization, and table queries are foundations to extend.
  They must not appear in an implementation plan as wholly new components.
- **Separate historical decoding from projection/index versioning.** Historical ABI storage/lookup
  already exists. The current-ABI RPC lookup in physical index setup does not establish the mapping
  identity, immutable revisions, fork/intrablock rules, or publication guarantees accepted in D13e–g.
- **Extend configured-table automation to account-scoped full auto-map.** The inspected state queue
  path still checks the configured table. The target removes manual table/field maintenance for
  allowed accounts; this broader discovery/activation behavior needs explicit integration evidence.
- **Extend the index policy deliberately.** Existing `auto_index` creates field indexes. The accepted
  D13j direction adds automatic selection/resource policy and easy operator defaults; it is not a
  claim that those policies already exist.
- **Consolidate shared semantics behind backend interfaces.** Mongo index setup, state writes, and
  table queries currently use Mongo-specific operations. Portable definitions and WormDB support
  need matching logical contracts and capability validation, rather than duplicated definitions.
- **Qualify stronger activation and recovery guarantees.** Worker config reload and pause/resume
  synchronization provide useful mechanisms; they do not by themselves establish staged generation
  rebuilding/publication, per-projection coverage, or recovery after interrupted activation.

Existing Mongo code is evidence for the design, not the final backend-independent specification.
In particular, the current route accepts Mongo-shaped JSON filters; which public query semantics
must be portable remains open under D02/D07. The existence of `/v2/state/get_table_rows` does not
establish compatibility with native `/v1/chain/get_table_rows`.

The indexer's master and the CLI synchronizer also contain separate index-creation logic. The
backend-interface design should reconcile their differences instead of assuming that the current
paths already implement a single index contract.

The reader requests reversible blocks and has fork detection/cleanup. The inspected
`src/indexer/workers/state-reader.ts` method `deleteForkedBlockById` removes Elasticsearch block,
delta, and action documents. Combined with the in-place Mongo dynamic-state writes described above,
this does not establish complete Mongo state restoration or query isolation during rollback. D05
requires those guarantees to be qualified for the chosen backend; D06 leaves query visibility during
recovery open.
