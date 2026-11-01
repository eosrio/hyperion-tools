# Retained Wire Linux deadline capture (#83)

This source-only diagnostic prepares evidence for the repeated HAPI #404 retained-input
timeout. It does not repair the test, qualify a physical MongoDB replay, or prove full Wire
testnet sync. MongoDB remains the 4.5 state backend and Elasticsearch the history tier.

The original failing command is `node --test tests/unit/wire-managed-state-v2-native.mjs`,
with `HYPERION_NATIVE_SOURCE_PROFILE=stateful-owner-v2-dd85ee5-18d235e-fields-engineering`.
The test's unchanged interleaved 1..1480 body has its existing **180000 ms** timeout.
Workflow `41256269394`, Node24 job `124699981577` observed Node **24.22.1**, npm **11.19.1**,
Ubuntu **24.04**, product `21ef67d84ad94fb35a0c1bba600a42df5318c404`, tree
`95a331dbe4ba2b9438db7afabef35f57c35fe032`. Node26 was cancelled, not passed.
The earlier passing #401 workflow is a separate observation, not a timeout repair.

The path-scoped workflow checks out that exact product with persisted credentials disabled,
normal-installs its unchanged committed lockfile, checks Bun1.4.4/TypeScript7.0.2, freshly
builds core and adapter, then runs the four native prerequisites that preceded the failed
test in the original job. It intentionally does not add the separate adapter `npm ci`
used by the #82 dependency artifact workload. This diagnostic neither consumes nor produces
that dependency archive. Runtime pins and results stay separate.

## Capture and result semantics

The driver adds only a diagnostic `--import` and TAP reporter to the original test command.
It does not transform source, replace modules/functions, add a deadline, retry the test,
start a debug listener or send a process signal. The preload selects the actual isolated
test process using the exact target argv, `NODE_TEST_CONTEXT=child-v8`, thread and process
identity. Its PID must differ from and directly descend from the captured runner PID.

A CI-only 250ms canary first proves actual Node24 preload propagation, a sampled canary
stack, and final in-process inspector flushing. This actual-runtime probe is distinct from
the source-only offline controls. It establishes instrumentation behavior, not Hyperion
performance. The exact runtime help and embedded test-runner implementation are retained
by value for review.

Inside the real test worker, a local `inspector.Session` samples V8 at1000us and stops/writes
each30s chunk, then starts the next chunk. A natural `beforeExit` flushes the final chunk.
At most seven chunks are captured; exceeding that bound explicitly fails lifetime coverage.
Completed profile files are written atomically with interval/PID receipts. Rotation gaps
are retained explicitly. One-second cumulative CPU/RSS/resourceUsage and raw `/proc/self`
stat/status/io samples belong to this worker, not just the test-runner parent.

`report.json` records independent outcomes:

| Field | Meaning |
|---|---|
| `workload.verdict` | Original assertions and180000ms timeout plus natural wait and stream closure. Any timeout/signal/failure remains FAIL. |
| `coverage.verdict` | Actual-worker identity, parseable complete CPU chunks, sampled **real HAPI build and SDK URL stacks**, final flush/exit receipt. Missing/partial/parent-only captures fail. |
| `coverage.censoredInterval` | An unfinished final interval after timeout/reap or missing natural exit; completed earlier chunks remain available for analysis. |
| `collection.verdict` | Driver reached collection and custody checks. PASS alone never means the test or coverage passed. |

The command returns nonzero unless all three outcomes pass. There is no `continue-on-error`.
`always()` artifact upload retains failures. CI's outer15-minute job bound is an emergency
failure boundary, not an increased test deadline. If the runner is cancelled before final
collection, the durable report remains INCOMPLETE; no upload outcome can promote it to PASS.

Source/tree/lock/test/fixture hashes, installed package metadata, fresh emission hashes,
normal install/typecheck/build/control argv and raw logs, exact Node executable hash,
CPU topology, host/cgroup views and runtime versions are retained. Commands record start,
exit, close, each stream end, byte counts and raw log hashes. Output is bounded to64MiB per
stream; any truncation fails the phase rather than silently passing. The source diff must
remain clean before and after. `SHA256SUMS` covers the resulting owned evidence directory.
Absent `/proc` or cgroup files are recorded as unavailable, never inferred green.

CPU self-sample tables report sampled microseconds, not exact exclusive CPU. JavaScript
profiles do not resolve all native stacks, allocations, I/O or external-service causes.
The existing nested/parallel wall timers are not additive CPU. No throughput or integrated
speedup claim follows from this capture. Once actual Linux evidence exists, use measured
stacks to derive a faster regression that still exercises the real product path.

## Verified primary semantics

- [Pinned Node24 test runner source](https://github.com/nodejs/node/blob/v24.22.1/lib/internal/test_runner/runner.js):
  `filterExecArgv`/`getRunArgs` retain the diagnostic preload; `runTestFile` creates the isolated
  worker with `NODE_TEST_CONTEXT=child-v8`.
- [Node24 preload documentation](https://nodejs.org/docs/latest-v24.x/api/cli.html#--importmodule):
  preloads propagate to forked/clustered workers; the CI canary verifies the actual pinned binary.
- [Node24 inspector CPU example](https://nodejs.org/docs/latest-v24.x/api/inspector.html#cpu-profiler):
  local `Profiler.enable/start/stop` returns a CPU profile without opening a network listener.
- [Pinned native profiler source](https://github.com/nodejs/node/blob/v24.22.1/src/inspector_profiler.cc):
  CLI `--cpu-prof` writes on exit; bounded inspector chunks avoid depending entirely on a
  worker surviving until that flush. We do not assume abnormal termination flushes a profile.

The live24.x documentation can move; retained actual help/source and canary receipts bind
each run to24.22.1. No broad local Windows suite is needed to prepare this capture:

```powershell
node --check scripts/wire-managed-linux-profile/run.mjs
node --check scripts/wire-managed-linux-profile/worker.mjs
node --test scripts/wire-managed-linux-profile/offline.test.mjs
```

Offline tests use synthetic profile/identity/verdict objects only. They neither import the
product, launch workload children, install/build dependencies nor contact a host or service.


## Fixed Node 26 failed-environment case (#86)

The `node26-404d` case targets HAPI commit `404d36f58aaaf83b5b25c8e5a62d38ed582f5d57`
and tree `df4e4527bc6a3a34c5f046b5f6f5f192c382b66e`, Node 26.13.0 and npm 11.20.0,
matching failed workflow `41262118567`, job `124717329561`. Its separate workflow, output
directory, artifact and `report-node26-404d.json` distinguish this case. The original
`node24-21ef` case remains explicitly dispatchable with its original report filename.
Pull requests touching the shared harness run the Node 26 case only; the original workflow
remains available through manual dispatch. Both use the same unchanged workload, prerequisites,
preload canary, worker, coverage and artifact gates. The driver accepts only these fixed case
IDs; it refuses arbitrary source refs and runtime selections. These source changes alone
establish no runtime regression, improvement or physical replay result.
