# HAPI Linux dependency input producer

Scoped to [Tools #82](https://github.com/eosrio/hyperion-tools/issues/82).
The workflow prepares dependencies for the independently source-verified HAPI product
`21ef67d84ad94fb35a0c1bba600a42df5318c404`, tree `95a331dbe4ba2b9438db7afabef35f57c35fe032`.
It does not publish a built HAPI application or grant qualification admission.

The path-scoped workflow runs offline controls, checks out that exact product with persisted
credentials disabled, selects Node 26.4.0 and Bun 1.4.4, and verifies TypeScript 7.0.2. Before
and after installation it verifies all 1,041 tracked source files and the four manifest/lock
byte hashes in `profile.json`. Its source inventory digest is
`38774ca1c5974997b4d047afa72c262d1372f871b26760d526ec49bab355096b`.

Both installs use their committed locks, normal lifecycle scripts and invocation-scoped Git
permission. Their working directory is the product checkout:

```text
<actual node> <actual npm-cli.js> ci --allow-git=all --ignore-scripts=false
<actual node> <actual npm-cli.js> ci --allow-git=all --ignore-scripts=false --prefix packages/backend-mongodb
```

The producer retains the actual child wait and drains both original pipes to EOF. The receipt
contains exact argv, runtime/npm versions, terminal results, and raw stdout/stderr as base64
with byte counts and SHA-256. An unsuccessful install, changed source, open/failed stream or
exceeded bound cannot produce eligible inputs. There are no command retries or process stops.
Normal install scripts may emit application files in CI; those files are never archived.

The fixed artifact `hapi-dependencies-21ef67d84ad9-linux-x64-node26.4.0` contains exactly:

- `candidate82-dependencies.tar.gz`
- `candidate82-dependencies.receipt.json`

The tar contains complete `node_modules/**` and
`packages/backend-mongodb/node_modules/**`, plus the directory scaffolding `packages` and
`packages/backend-mongodb`. It preserves original file bytes and safe modes. Directories use
0755 and symlinks use 0777; inventory rows have the agreed exact typed field shapes. Git and
credential paths, hardlinks/devices, unsafe paths, missing/cyclic/escaping links, extra payload,
and application `build/` or backend `dist/` adoption are refused. Bounds are 30,000 dependency
entries, 2 GiB payload, 256 MiB per file, 1 GiB archive and 16 MiB receipt.

The only link into product source is the explicitly declared backend workspace link:
`node_modules/@eosrio/hyperion-backend-mongodb` → `../../packages/backend-mongodb`.
Its target is bound to the complete independently verified product source digest. The archive
does not follow it. All other links must terminate within the complete dependency inventory.

The receipt records the committed root/adapter SDK and Wire pins, package JSON hashes, Wire's
prepared SDK revision and its four untouched original artifact/source/npm-lock receipts.
Every original node-abieos `.node` file is pinned, including other prebuilt platforms.
Dependency compilation is distinct from core/backend application emission: the isolated
consumer must verify these immutable inputs and then freshly compile all application outputs
and run its own source/runtime controls.

The schema is shared with the private qualification consumer. No private host, guest, route or
qualification contract is included here. `producer.workflowRevision` identifies this Tools
workflow; `producer.sourceHead` identifies the HAPI product. GitHub's uploaded artifact ID and
ZIP digest are reported separately after upload. Root must independently freeze those API
identities and the two input file hashes before admitting a guest input. Receipt/archive pins
remain unresolved until that review. See the official [workflow variables](https://docs.github.com/en/actions/reference/workflows-and-actions/variables)
and [artifact outputs](https://github.com/actions/upload-artifact) contracts.

Offline checks require only Python's standard library and perform no npm installation:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/hapi-linux-dependencies -p 'test_*.py' -v
```

The POSIX filesystem writer/link test runs in Linux CI; archive, receipt, link-graph, source,
lock, stream and workflow controls also run on Windows. Local source controls do not establish
that a Linux dependency artifact exists or that the full Wire/MongoDB replay passes.


## Current source case (#88)

The separately selected `current88` case pins HAPI source `f16351b774409802f71d08b3f49c6c5f35c9f63a`
and tree `7849f9caf466b49e69c6896f797d22a239641de8`. Its profile freezes the complete source
inventory, four manifest and lock hashes, dependency identities, and Wire prepared SDK revision.
An unresolved or incomplete case refuses before source access or installs. Its separate workflow
checks case readiness before checkout and uses distinct output, archive,
receipt, and artifact names. The default `candidate82` case and its published profile and
artifact identity remain intact. The current case authorizes no guest or service operation.

Consumers validating the current receipt must select the case and
pass both its fixed profile and identity to `validate_receipt`; the no-argument verifier
continues to admit only the original case. Case selection does not accept caller-supplied
source refs, profiles, runtime pins, or commands.


## Current103 source case (#90)

The separately selected `current103` case pins HAPI commit `bca518ddfebf04c2af957832f2ad9c0582c17932`
and tree `26cfd37e658d030090059096afc32a574ed787ef`. Its independently declared source
inventory differs from current88; the four manifest/lock and dependency source identities remain
unchanged. The dedicated workflow produces distinct archive, receipt and artifact names, while
current88 remains intact. This source profile alone grants no guest operation or runtime qualification.
The new producer artifact, API identity, receipt and archive bytes remain pending independent review.
