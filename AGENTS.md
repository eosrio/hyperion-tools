# Hyperion 4.5 engineering guidance

Before changing 4.5 behavior, read [the architecture definition](docs/architecture/hyperion-4.5.md),
the applicable [ADRs](docs/adr/), and [the delivery roadmap](docs/roadmap/hyperion-4.5.md).
Use [CONTEXT.md](CONTEXT.md) for shared terms.

For performance or runtime work, follow the
[component profiling program](docs/performance/hyperion-4.5-profiling.md). Use guarded,
ABI-correct workloads and publish stage/resource evidence before selecting an optimization.
Track Bun runtime qualification and Elasticsearch release changes separately from toolchain checks.

- Work from a scoped issue in the repository that owns the change. Coordinate cross-repository
  work through the release issue linked from the roadmap.
- Preserve existing Elasticsearch history through upgrades; initialize new state projections separately.
- Keep protocol extensions, contract definitions/hooks, backend adapters and API domains behind
  their accepted interfaces. Definitions and logical queries remain backend-neutral.
- Preserve the existing worker infrastructure and RabbitMQ default. Contract failures remain isolated;
  durable effects, recoverable progress and canonical fork recovery govern successful processing.
- Keep implementation evidence separate from release qualification. Do not claim AtomicAssets parity,
  Wire testnet support or WormDB qualification from a partial fixture or benchmark.
- Keep approved release constraints and superseded decisions explicit. Return scope or public-behavior
  changes to architecture discussion instead of silently weakening acceptance criteria.

The SDK and extensions release independently. The catalog declares compatibility; it does not
authorize automatic executable updates. Operators choose and audit installed executable plugins.
