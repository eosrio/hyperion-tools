# Hyperion 4.5: SDK and component foundation

Implementation foundation for [P01 / tools #19](https://github.com/eosrio/hyperion-tools/issues/19).
This assigns the initial package and source homes within the accepted
[architecture](hyperion-4.5.md). A source-home assignment is not an implementation or support claim.

## Initial package and source homes

| Responsibility | Package identity | Source home | Release ownership |
|---|---|---|---|
| Public authoring and extension compatibility contracts | @eosrio/hyperion-sdk | eosrio/hyperion-sdk | Independent SDK major/version; development package until qualified. |
| Existing Node worker/runtime and API-host integration | Existing hyperion-history package | eosrio/hyperion-history-api, existing indexer and API modules | Hyperion core release. No additional service or runtime package is required by this foundation. |
| Shared Rust ingestion, protocol, snapshot and replay utilities | Existing hyperion-tools workspace crates, including hyperion-ship | eosrio/hyperion-tools | Retain current crate ownership; language bindings share the SDK's declared semantics where applicable. |
| MongoDB backend adapter extension | @eosrio/hyperion-backend-mongodb | eosrio/hyperion-history-api, packages/backend-mongodb | Independent adapter package version, distinct from the MongoDB engine version. |
| WormDB backend adapter extension | @eosrio/hyperion-backend-wormdb | eosrio/hyperion-history-api, packages/backend-wormdb | Independent adapter package version, distinct from the WormDB engine version. |
| WormDB engine and engine-specific native capabilities | Existing WormDB packages/crates | igorls/wormdb | Database project ownership; native integration details follow capability evidence. |
| AtomicAssets and AtomicMarket | @eosrio/hyperion-atomicassets-plugin | eosrio/hyperion-atomicassets-plugin | One independently released operator package with modular internals. |
| Wire chain extension | @eosrio/hyperion-wire-plugin | eosrio/hyperion-wire-plugin | One operator package coordinating required Node/Rust artifacts and versions. |
| Compatibility catalog and tested-set records | Catalog format/schema version, not a runtime npm package | eosrio/hyperion-plugins | EOS Rio catalog maintenance and explicit compatibility evidence. |

The adapter directories are assigned homes for subsequent slices, not existing implementations.
The first portable MongoDB slice will factor native operations out of existing paths into its adapter.
The WormDB adapter's native/transport integration follows the capability assessment; engine work
stays in WormDB. Neither adapter belongs in the SDK, and definitions do not duplicate native queries.

Keep lifecycle scheduling, source coverage, activation and fork coordination in shared Hyperion
runtime code. Adapters implement physical storage/query/rollback. Protocol codecs run before
contract processing; the package layout does not change these boundaries or prescribe IPC.

## SDK foundation boundary

The first SDK package establishes component manifest validation and compatibility eligibility.
It does not predefine speculative mutation, query, checkpoint or worker APIs. Those contracts grow
through the [first real projection slice](https://github.com/eosrio/hyperion-sdk/issues/1).

Manifest schema versions describe metadata format. SDK package versions describe public SDK
contracts. Adapter package versions and required database-engine versions are separate identities.
Version ranges express declared eligibility; passing a range check is not release qualification.
Tested component combinations require exact pins and explicit evidence in the catalog.

Development SDK packages stay private and are not published as stable releases by foundation CI.
Breaking public SDK contracts require a new major; compatibility within a major remains required.

## Existing plugin migration

Keep @eosrio/hyperion-plugin-core and the existing loader path available while new SDK integration
is developed and tested. This foundation does not replace the dependency or reinterpret installed
legacy plugins as new SDK components.

The [SDK migration guide](https://github.com/eosrio/hyperion-sdk/blob/8be63ea590b015d8c56a1269afc2fe4be3c8a1e0/docs/migration.md)
records inspected legacy consumers and their source baselines. Existing
package-based plugins and older source-relative plugins require different migration work. Preserve
their supported legacy path until each selected migration is explicitly qualified. No automatic
source conversion, sandbox guarantee or plugin compatibility is implied.

## Catalog rollout

Keep the legacy catalog.json and its consumers unchanged. Introduce versioned compatibility
metadata separately, using the SDK's canonical manifest contract. New records must distinguish
declared ranges from tested combinations and retain exact component source/version pins and evidence.

The catalog begins without qualified 4.5 release entries. Synthetic validation fixtures are not
support declarations. Catalog changes neither install code nor update running executables.
Operator installation/activation integration is later host work, not part of metadata validation.

## P01 completion evidence

P01 requires the source-home table, source-backed legacy migration guidance, a documented catalog
format with validation fixtures, and links across the participating repositories. Merge the relevant
SDK/catalog/documentation changes before treating this foundation as integrated. It does not close
SDK #1, qualify MongoDB/WormDB, establish AtomicAssets parity or qualify the Wire testnet.
