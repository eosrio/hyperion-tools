# Hyperion 4.5 TypeScript toolchain

Status: Accepted engineering baseline, updated 2026-10-24.

Use TypeScript 7 and `bun check` for ongoing TypeScript development in Hyperion 4.5.
This policy applies to the History API, shared SDK, and new TypeScript plugins and backend
adapters. Their package and source ownership remains defined in the
[component foundation](hyperion-4.5-component-homes.md).

## Pinned baseline

| Responsibility | Baseline |
|---|---|
| Project compiler and editor types | Exact `typescript` dependency `7.0.2` |
| Fast project typecheck | Bun `1.4.4`, pinned in each migrated repository's `.bun-version` and installed from that pin in CI |
| Dependency installation | `npm ci` using the committed `package-lock.json` |
| Emitted JavaScript, source maps and SDK declarations | The pinned TypeScript 7 compiler through the repository's existing build scripts |
| Minimum Node runtime for 4.5 | Node 24, declared in owned package engine constraints and lock metadata |
| CI Node runtime acceptance | Node 24 and Node 26 build/artifact and test coverage |

Bun's checker uses its embedded TypeScript version, independently of the project's installed
compiler. Keep `bun -p process.versions.typescript` equal to the exact `typescript` dependency;
select the workspace TypeScript version in editors. Review Bun and compiler pin updates together.
Bun 1.4.4 was verified to embed TypeScript 7.0.2. CI verifies both the Bun pin and embedded
compiler before checking the complete project.
[Bun documents the version relationship and checker behavior](https://bun.sh/docs/runtime/check#bun-check-and-my-editor-disagree).

## Development and CI

From a TypeScript package's directory, install and validate with its documented scripts:

```sh
npm ci
npm run typecheck
npm run build
```

The standard `typecheck` script is `bun check -p tsconfig.json`. It checks the whole project
selected by that configuration and is required in CI as well as local development. Projects with
multiple TypeScript configurations must check each applicable configuration, or use `bun check -b`
with explicit project references. Include dynamically loaded entry points and workers in the
appropriate project configuration. Checking only changed files or the imports reached by a test
does not replace this gate.

Keep the native `tsc` build for emitted artifacts. `bun check` does not emit JavaScript, source
maps or `.d.ts` files; passing it is insufficient build acceptance.
[Bun documents these emission limits](https://bun.sh/docs/runtime/check#differences-from-tsc).
Validate the generated Node artifacts and the SDK's packed exports and declarations through their
existing tests on Node 24 and 26. Preserve any existing Bun-based test coverage alongside those gates.

The toolchain change does not change dependency installation or require Bun as the production
runtime. Keep `package-lock.json` authoritative; install dependencies with npm even when running
Bun's checker. Package scripts and CI must use the pinned checker and compiler consistently.
Do not add Bun runtime declarations to Node source merely to make the checker pass. Keep
`tsconfig.json` settings aligned with the package's actual runtime and public module format.

## Repository scope and migration evidence

New TypeScript plugin and adapter packages start on this baseline. Existing packages migrate with
a clean npm installation, a complete project check, artifact emission and their relevant runtime or
consumer tests. Correct configuration changes and source errors explicitly; do not hide a migration
failure by excluding affected source, suppressing diagnostics or weakening established checks.
Report any remaining failure and its evidence before claiming that repository migrated.

Rust crates retain their existing toolchain and validation. The JavaScript compatibility catalog
retains its metadata validators and tests; it needs no empty TypeScript project or checker gate.
Catalog compatibility metadata describes declared eligibility and tested combinations, while
runtime and artifact tests establish execution behavior. A compiler upgrade alone does not qualify
a plugin, backend, chain protocol, or API domain for the release.

## Node 24 minimum and Node 26 CI

The operator selected Node 24 as the minimum for Hyperion 4.5 and replaced Node 22 with
Node 26 in maintained CI. [The coordinated change](https://github.com/eosrio/hyperion-tools/issues/65)
tracks the History API and MongoDB adapter, SDK, Wire package, stream client and catalog tooling.
Keep runtime-specific builds, installed-package consumers and applicable real-service checks on
Node 24 and 26. Node-independent checks may run once; Node/Rust comparisons still run on both.
Documentation-only packages and Rust-only crates retain their applicable checks.

Cancel superseded PR validation runs without cancelling push or manually requested runs.
Replacing one matrix version alone does not reduce job count; distinguish expected savings from
measured CI duration. Keep historical Node 22 source/runtime labels and evidence unchanged.
CI adoption does not upgrade production hosts or establish full worker, backend or release acceptance.

## Bun 1.4.4 adoption boundary

The operator selected Bun 1.4.4 for ongoing development on 2026-10-22 in
[the coordinated toolchain update](https://github.com/eosrio/hyperion-tools/issues/61).
Keep previous 1.4.3 measurements and reports at their original versions. This checker update
preserves TypeScript 7.0.2, npm locks, emitted Node artifacts and the then-current Node 22/24 validation.

Checker adoption does not qualify Hyperion to run under Bun. A
[read-only Linux client observation](../performance/evidence/bun-es-client-20261022.json)
with its [exact probe](../performance/evidence/bun-es-client-20261022-probe.mjs) records
Node 24 success and Bun 1.4.4 failures with the locked Elasticsearch client against the owned
9.5.5 fixture. This observational artifact is separate from test discovery and release acceptance.
Runtime prerequisites remain tracked in
[History API #197](https://github.com/eosrio/hyperion-history-api/issues/197) and the
[profiling program](../performance/hyperion-4.5-profiling.md).
