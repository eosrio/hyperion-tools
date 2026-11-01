# current103 packed registry supplement

The automatic producer now selects the separately named **private-pidns-v2** workflow/profile.
The original v1 profile bytes and failed runs remain historical inputs; its workflow is manual-only.
The v2 Linux launcher uses existing `sudo`/`unshare` and a source-pinned stdlib PID1 reaper, mounts a
private procfs, checks namespace/init identity, drops back to the runner's original UID/GID and
sets inherited no-new-privileges. No host-process fallback is permitted. The original all-new-process
guard remains strict inside that owned namespace, including escaped/reparented children and zombies.

The failed 8a run's npm child exited zero and reached EOF, but its wrapper timed out with incomplete
closure. Its blocking census was not captured; a particular unrelated process is not an established
cause. V2 adds fixed start, direct-exit, EOF and terminal observations, a five-second mutable last-census
diagnostic slot, complete final process/birth/state/reason records and a separate pre-teardown context
completion receipt. Census coverage is bounded to 4,096 processes and excess/unknown scope refuses.
Diagnostics consume the original command caps. Original failure and unknown closure stay failed even
if namespace teardown later removes processes. Kernel teardown is never used as successful closure.

V2 carries only the required CI/PATH/lifecycle-tracking environment into the builder, records existing
launcher binary identities and verifies the same exact Node/npm/Bun/TypeScript/source inputs. A successful
producer receipt also needs the separate context proof before its four-file artifact can be considered.
Actual namespace execution and v2 cache/artifact qualification remain pending Linux CI and Root review.
No Wire resource, workload, deadline or release requirement changes with this CI isolation.

This is the producer and read-only contract slice of [Tools #94](https://github.com/eosrio/hyperion-tools/issues/94).
It leaves the current103 two-`node_modules` artifact, producer, profile and receiver unchanged.
It does not implement receiver installation or grant permission to continue a Wire case.

The fixed product is HAPI `bca518ddfebf04c2af957832f2ad9c0582c17932`, SDK741 and Wire69.
The original `packages/backend-mongodb/scripts/check-package.mjs` is verified by raw source inventory and
its own 3,335-byte SHA-256. Its source, normal pack/install command, declarations and all 56 controls
are run unchanged. No old product build or local source tarball is admitted as an input.

The root and backend SDK scopes are verified separately. Both BCA committed locks and the admitted
current103 receipt select SDK741 in those two scopes; Wire69's own prepared SDK is58. The original
consumer packs the backend's installed SDK, not the root SDK path. Older private f163/native429 and
public HAPI419/SDK49 assemblies are separate inputs and are not substituted here.

## Exact inputs and producer sequence

The Linux/x64 workflow selects Node26.4.0, npm11.17.0, Bun1.4.4 and TypeScript7.0.2.
Version labels alone are insufficient: before loading any npm cache API, the producer verifies the
Node executable and every name, type, mode and file hash of the actual 2,376-member npm bundle.
`npm-members.json` is an exact subset of the independently captured full Node manifest
`98fefd07bd7b5faf4eceae81127da0d22a610e2c0b225ff085446b6a8666d7bd`.
Its full-manifest provenance and raw subset hash are in the named profile. No platform fallback or
line-ending normalization is performed. Local Windows npm is not a baseline for admission.

The producer makes normal committed-lock installs, both whole-project Bun checks and fresh native/root
builds in a clean source checkout. It rehearses the original empty consumer and its two fresh local
packs plus `@types/node@25.5.0`, retaining the actual lock, full installed inventory and source provenance.
It traverses every dependency, optional dependency and peer resolved by that lock. A missing required
node, unreferenced package, foreign local/Git source or changed fixed source root is refused.

The actual pinned npm cache API exports only reachable public registry metadata and selected transitive
tarballs into a new cache. It records exact keys, original/export times, metadata, SHA-512 SRI and cache
index/content member inventories. The Python contracts independently verify bodies, metadata-to-lock
and tarball-to-lock identity, every package member, cache references and the complete archive. Credentials,
local BCA/SDK packs, build outputs, arbitrary cache namespaces, unrelated bodies, links and unsafe members
are rejected. The broad rehearsal cache is never exported wholesale.

The seed is sealed before the proof. A second checkout is materialized from the exact source Git objects,
with another normal install and fresh build. The verified sealed seed is copied once into a fresh mutable
cache. The unchanged packed consumer receives `npm_config_offline=true` and the specifically named
`npm_config_update_notifier=false`, an owned config/cache/tmp/home and the original native source profile.
Its local source packs are made again. Source, runtime and sealed-seed inventories are rechecked afterwards.

The actual two selected local pack bodies are collected from that proof's mutable cache only after the
original command has completed. They are compared member-by-member to the fresh adapter/SDK source and
emissions and retained in a separate `OUTPUT` archive. These are measured outputs, never seed members.
The observation of the consumer's temporary directory binds relative local-file cache keys; it neither
rewrites the packed consumer nor intercepts its npm calls.

## Evidence and limits

The new workflow produces three files only after success: a registry seed tar, a receipt and a separate
OUTPUT tar. Raw command receipts include argv, cwd, cap, original wait result, bounded raw stdout/stderr
and hashes, pipe EOF, process birth identities and two bounded observations of descendant closure.
Every process name is considered; a newly observed unrelated process also causes refusal rather than
being silently exempted. A timeout, uncertain closure or metadata error is a failed producer. No signal,
process stop, retry or cleanup exception is used. Failed receipts remain diagnostic artifacts; upload
success cannot turn the failed command into a passing proof.

The isolated packed command retains its original 60-second ceiling. A timeout is never extended to finish
collection. Command preparation has its own explicit producer caps, including the existing 1,200-second
normal install caps; these producer steps do not claim to fit the retained Wire component envelope.
Each command records CPU accounting, maximum child RSS and observable cgroup files. Cumulative child
accounting and hosted-runner observations are not exclusive CPU profiles or receiver resource qualification.
The producer refuses to start or continue major stages below the original 45GiB free reserve.

The receiver's original 300-second component envelope, command limits, 6CPU/16GiB/swap0 resources,
96GiB disk profile, 1..1480/180000ms workload and closure/postguard requirements remain unchanged.
This workflow does not run that component. `fullWireComponent` is **NOT_RUN**. No headroom, replay success,
physical Mongo qualification or release qualification is inferred from an isolated packed proof.

Source profile artifact pins remain `null`. The immutable receipt does not claim its own post-upload
Actions identity. Root must independently verify the actual API workflow/run/job/artifact identity, ZIP,
receipt, tar bytes and all member/source/runtime inventories before a separately reviewed receiving
integration can select them. No example, validator return value or boolean grants host admission.

## Safe local controls

The finite tests import only the stdlib data contracts. Their generated tar/cache/lock/receipt examples
are modeled input, not evidence of an actual npm resolution, Linux cache or CI run:

```powershell
python -m unittest discover -s scripts/hapi-packed-registry -p test_contracts.py -v
node --check scripts/hapi-packed-registry/cache-bridge.mjs
```

`producer.py` and `cache-bridge.mjs` are Linux CI entry points. Do not import or execute either on Windows.
Syntax parsing is allowed; it does not execute their bodies. No local database/controller/timeout fixture
or legacy process-stop helper is part of these controls.
