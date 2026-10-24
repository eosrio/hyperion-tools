# Hyperion 4.5 component profiling program

Status: Accepted engineering workstream, 2026-10-18. The operator requested deep profiling
across the stack and identified Bun 1.4.3 as a preferred runtime candidate. Coverage and
qualification below are work to deliver; this document is not a completed performance report.

## Outcome and ownership

Maintain a coverage ledger for every shipping process, native component, plugin domain,
backend and external dependency. Each row links its exact workload, correctness oracle,
profiles, resource measurements, result and next experiment. A component without evidence
is marked **not measured**; an unfinished component is marked **not implemented**. Neither
is counted as a passing baseline. Complete the applicable ledger before release qualification.

Use the [accepted architecture](../architecture/hyperion-4.5.md) and
[release matrix issue](https://github.com/eosrio/hyperion-tools/issues/20).
Keep the [TypeScript/Bun authoring policy](https://github.com/eosrio/hyperion-tools/blob/1a38d018cbc25aef3ac35636e11761ef8c9863f3/docs/architecture/hyperion-4.5-typescript-toolchain.md)
separate from production-runtime qualification: a successful typecheck or Bun-driven CLI
does not establish that the indexer or API runs on Bun.

Tools owns the cross-component protocol and evidence manifest. History API owns its worker,
queue, runtime, API, cache and Elasticsearch integration instrumentation. SDK and plugin
repositories own their execution slices; database engines own native engine profiles.
Link operational defects to [History API #187](https://github.com/eosrio/hyperion-history-api/issues/187)
rather than duplicating its resilience backlog.

## Coverage ledger

Apply CPU sampling and allocation/retention analysis inside each process as well as elapsed
stage timings. Include native stacks and operating-system I/O; a fast JavaScript frame does
not account for native execution or blocked time.

| Component | Required observations and representative workload |
|---|---|
| Chain source, SHIP and disk reader | Receive/decompress/parse/serialize costs; bytes, feed ceiling, blocked sends, reconnect and canonical block identity. Compare network replay with direct disk only as separate architectures. |
| Worker scheduler and IPC | Per-role CPU, queue residence/age, serialization/copies, event-loop delay, IPC bytes, worker utilization/restarts and live priority during backfill. |
| ABI resolution and native/JS codecs | ABI cache hits/misses, block-valid ABI lookup, native and JS fallback frequency, decode failures, CPU/allocation and equivalent decoded output. Include ABI deployment transitions. |
| Block/delta/action processing | Per-block and per-row work, contract mapping/hooks, action enrichment, nested spans, bounded independent partitions and head-of-line blocking. |
| RabbitMQ and shared-reader backpressure | Publish/deliver/ack/unacked rates and bytes, queue age, credit/prefetch, confirmation wait, memory/disk flow alarms, management-API failure and pause/resume behavior. |
| Elasticsearch write path | Serialization, actual receiving node, connection wait, bulk/item latency and failures, retry/acknowledgment, durable progress, refresh visibility, pressure, merges, flush/translog and replicas. |
| Elasticsearch read path | Route-real filters/sorts/counts/deep pagination, shard fan-out, server CPU/heap/breakers, empty/timeout shards and concurrent reads/writes. Separate client wall time from server execution. |
| MongoDB adapter and engine | Mutation/query families, pool/lock waits, indexes and explain plans, batched ordering, retry/partial writes, durable progress, rollback and initialization contention. |
| WormDB adapter and engine | The same applicable projection/query semantics, plus transport, locks, allocation, WAL/fsync, compaction, recovery and index costs. Record durability mode. Existing KV benchmarks are not projection qualification. |
| Redis and application caches | Hit/miss ratios, command/pipeline costs, key cardinality/TTL, allocation/eviction, persistence/disk churn, cold/warm behavior and catch-up/live interference. |
| REST API and response encoding | Route-family p50/p95/p99, queueing, auth/validation, backend/cache stages, serialization/compression, bytes, concurrency, errors and timeouts. Preserve compatible JSON. |
| Streaming and control transports | Ingress-to-client freshness, subscriptions, fan-out, slow consumers, buffered bytes, disconnect/reconnect, native server costs and controller worker coordination. |
| SDK and contract plugin execution | Mapping/projection/hook/query cost and allocation, initialization, version changes/reload, failure isolation and shared-header serving. Metadata checks alone do not cover execution. |
| AtomicAssets and AtomicMarket | Domain-real mutation/query families, joins/enrichment, sorting/counts/pagination and live ingest plus serving, preserving the pinned full parity obligation. |
| Wire protocol extension | Actual selected protocol/frame/ABI/key differences across required Node/Rust paths and the new testnet profile. Include IPC/native crossings and equivalent results. |
| Rust ingestion, archive and snapshot tools | Read/inflate/decode/cache/map/serialize/write/index-build costs, native CPU/allocator and disk profiles, cold/random access, bounded hydration and bootstrap competing with live work. |
| Initialization, recovery and fork handling | Recovery-window size, replay/rebuild/rollback duration, memory/disk amplification, gated-query time and correct canonical state/progress after interruption. |
| Host, storage and network | CPU topology/limits, scheduling, RSS, page faults/cache, disk latency/IOPS/throughput/fsync, network and container overhead. Sample every contributing process and service. |

For each component retain counts and distributions, not only averages: input/output rows and
bytes; success, failure and retry counts; p50/p95/p99 and sample count; CPU self/inclusive
samples; elapsed busy and waiting time; RSS/native/external memory; allocations and GC;
queue residence; storage and network measurements. Treat unavailable metrics explicitly.
Node V8 and Bun JavaScriptCore heap fields have different semantics; do not equate their
reported heap limits or infer cross-runtime capacity from them.

## Existing evidence and measurement gaps

The pinned [History API profiler](https://github.com/eosrio/hyperion-history-api/blob/b571c5fb2e7c1422aa5bff31c45c1c7ac54caddc/src/indexer/helpers/profiler.ts)
records count and total elapsed wall time and reports every second. Its intervals include
awaited I/O and nested envelopes. They are not CPU samples, are not additive across nested
spans, and provide no latency distribution. Existing API query timing and route/status counts
are useful starting signals; add attribution and percentiles rather than declaring no visibility.

Reuse the tracked [performance report](https://github.com/eosrio/hyperion-history-api/blob/e821fdfa687471427b6248a68fc40cf2e6c36047/docs/performance/PERFORMANCE_REPORT.md),
[experiments](https://github.com/eosrio/hyperion-history-api/blob/e821fdfa687471427b6248a68fc40cf2e6c36047/docs/performance/EXPERIMENTS.md)
and [future roadmap](https://github.com/eosrio/hyperion-history-api/blob/e821fdfa687471427b6248a68fc40cf2e6c36047/docs/performance/FUTURE_ROADMAP.md)
as historical evidence. They do not substitute for the current release baseline. The locally
untracked dense report documents an ABI-version mismatch that changes decode/fallback work;
do not select an optimization from its early headline numbers alone.

Existing `bench.ts` and `profile-collect.ts` launch and inspect Node containers. Their driver
being invoked with Bun is not a Bun-runtime result. The benchmark also targets fixed test
indices, queues, container names and ports; introduce dedicated namespace/volume/port guards
before reusing it alongside another workload. Do not run its cleanup against shared state.

Reuse existing tools' ES-load, direct-reader, archive and served-domain harnesses and WormDB's
engine/WormWire benches. Standardize their manifests, correctness checks and result semantics;
do not replace working harnesses merely to make them uniform.

## First runtime experiment: observed, not qualified

The [portable runtime probe](../../scripts/profile-runtime-smoke.mjs) exercises bounded,
service-free prerequisites using the same installed History API dependencies. Only its HTTP
case binds a loopback ephemeral port. For each mode run both actual executables:

```sh
node scripts/profile-runtime-smoke.mjs cluster /path/to/hyperion-history-api
bun scripts/profile-runtime-smoke.mjs cluster /path/to/hyperion-history-api
```

Repeat for `uws`, `abieos` and `heap`. Exit 1 is a failed prerequisite; exit 2 is invalid usage.
Use an explicit Node 24/26 executable for new qualification under the current baseline, rather than assuming the executable named `node` is that version.

[Recorded evidence](evidence/runtime-smoke-2026-10-18.json) uses the TS7 migration source
`b571c5fb2e7c1422aa5bff31c45c1c7ac54caddc`, Node 24.19.0, Bun 1.4.3 and Windows x64:

| Prerequisite | Node 24.19.0 | Bun 1.4.3 |
|---|---|---|
| Cluster fork, worker identity and IPC round-trip | Passed | Passed |
| uWebSockets.js 20.67.0 native HTTP listen/request/close | Passed | Failed loading the native module: missing `napi_register_module_v1` |
| node-abieos 4.2.0-f7d5b45 ABI load and uint32 encode/decode | Passed | Passed |
| Heap-stat API returns numeric values | Passed | Passed, runtime-specific semantics |

This is a Windows prerequisite smoke, not Linux support, full worker lifecycle, production
load, restart/fork recovery or performance qualification. The controller/router's current
uWebSockets dependency blocks the tested full-stack Bun path. Investigate a portable transport
seam or another supported implementation, preserving protocol, subscription, backpressure and
shutdown behavior. Do not silently replace the transport or a native codec to obtain a speed result.

Bun remains the preferred candidate when correctness and measured behavior support it.
Keep a qualified Node control path. Record any remaining runtime-specific limitation explicitly
rather than making Bun adoption an unstated prerequisite for unrelated 4.5 features.

## Full-worker load checkpoint: 2026-10-19

[API #207](https://github.com/eosrio/hyperion-history-api/pull/207) adds the next finite
[#198](https://github.com/eosrio/hyperion-history-api/issues/198) experiment after merged
API #204/#205 and Tools #34. Its [immutable report](https://github.com/eosrio/hyperion-history-api/blob/32bc4f8407e4dac2c9b40750ff9e9eca21b01a8e/docs/performance/elasticsearch-9.5.5-workers.md)
and [evidence](https://github.com/eosrio/hyperion-history-api/tree/32bc4f8407e4dac2c9b40750ff9e9eca21b01a8e/docs/performance/evidence/es955-workers-20261019)
record two fresh Node 24.19 executions, each with 300 s of write/read/mixed phases,
plus warmup, three sink stalls and recovery. **95,960 actions** were verified exactly on
both data nodes; all six pause/resume waves and both process/resource cleanups passed.
Thirteen CPU profiles cover the master plus all 12 workers through final recovery.

Actual SHIP block headers traverse the production workers; synthetic action messages
separately feed the real action queue. Traces/deltas, contracts/state projections and
the HTTP API server are outside this run. A proxy gates bulk request forwarding before
ES dispatch; real responses are unchanged. This tests sink-completion delay and the
existing shared-reader policy, rather than native ES pressure admission or a hard queue
cap. The next pending historical range waits for resume while live reading progresses.
Range evidence observes the actual reader method, not packet delivery or full range completion.

| Evidence | Measured coverage |
|---|---|
| Workload/output | 48,260 control and 47,700 profile actions; exact IDs/sources separately checked on both copies; complete sorted pages with a known-visible read baseline |
| CPU/memory/waits | Per-PID CPU/RSS/heap and one-second event-loop samples; 13 saved profiles; backend JVM/GC, breakers, pressure, refresh, merges and storage counters |
| Recovery/cleanup | Three waves per run, live progress, no next range while paused, exact pending range invoked after resume; idle accounting and no surviving processes/broker work; all owned fixture resources removed |
| Validation | 222 unit tests, whole-project/runner Bun typechecks, TS7 build, 11 mocked lifecycle cases and 87 real observation/profile/shutdown assertions |

The control retains observer/proxy instrumentation. One sequential profile/control pair
with fresh clusters and uncontrolled host workloads cannot attribute overhead or rank
runtimes/capacity. Zero native rejection/breaker trips at this offered rate do not prove
headroom, leak freedom or a production bottleneck. Preserve the archived CPU profiles
and latency distributions as diagnostic evidence, then qualify representative dense
historical data through #195/#196 before prioritizing throughput changes.

[Reader fix #206](https://github.com/eosrio/hyperion-history-api/pull/206) and portable
Windows imports/log filenames unblock this path. Remaining negative/unknown confirms
and reader replay belong to [#208](https://github.com/eosrio/hyperion-history-api/issues/208)
and #187. Bun 1.4.3 still fails the pinned uWebSockets native import on this host, despite
node-abieos loading; there is no full-worker Bun comparison. Longer soaks, contract/API
paths, worker-loss replay, large-memory pressure and retained-index acceptance remain open.

## Guarded replay and reader recovery checkpoint: 2026-10-19

The next parallel slices use native Node 24.19.0 workers on Hyperion-1 Linux and
owned, disposable container services. The shared production host is suitable for
these correctness cases; the runs do not establish capacity or throughput.

- [API #209](https://github.com/eosrio/hyperion-history-api/pull/209) corrects action
  ABI selection and native type-cache invalidation across deployment revisions,
  including custom/current fallback contexts. Its extra block-valid lookups need
  representative profiling before selecting a performance change.
- [API #210](https://github.com/eosrio/hyperion-history-api/pull/210) qualifies seven
  broker/worker recovery cases and reproduces three baseline failures. Its
  [immutable report](https://github.com/eosrio/hyperion-history-api/blob/694d473257283a359a41ad55173049075a18b3ea/docs/performance/reader-recovery.md)
  separates successful publisher confirms, source credits and downstream durability.
  The scheduler uses actual methods through a focused seam, with no full-master or
  live fork qualification. All 17 observed processes and the owned fixture were removed.
- [API #211](https://github.com/eosrio/hyperion-history-api/pull/211), for #195, now has one
  passing finite known-genesis contract replay: 88 captured blocks, 87 replayed
  blocks, 12 real signed actions and two ABI deployments. Raw SHIP inputs, signed
  transaction membership/payloads, ABI deltas, decoded output and both physical
  Elasticsearch copies are checked. All 11 processes exited cleanly; independent
  postflight verified source/input hashes and three idle samples before cleanup.

The [replay report](https://github.com/eosrio/hyperion-history-api/blob/60942800094238317b5bb0ddec98199b04de3bab/docs/performance/guarded-contract-replay.md)
and [checksummed raw evidence](https://github.com/eosrio/hyperion-history-api/tree/60942800094238317b5bb0ddec98199b04de3bab/docs/performance/evidence/guarded-replay-20261019)
retain all nine excluded pilots alongside the successful run.

The bulk ledger observes 15 requests and 101 successful item outcomes, with no failed
items or repeated attempts. Initial receipts acknowledge one copy for 19 items and
two for 82; final identity on both copies is a separate observation. This run does
not establish two-copy acknowledgment for every original write or crash survival.

The contract run supplies verified historical ABI references into its new ABI index;
it does not qualify automatic ABI discovery, historical mainnet data, contract
computation, state projections or served API throughput. Its Redis checkpoint remains
null because qualification shutdown bypasses production checkpoint persistence.
Keep #195 open for a persisted checkpoint and restart observation; do not count the
output oracle as completed recovery acceptance. Failed harness pilots remain excluded.

Keep #208 open for integrated live fork/restart recovery and broker-delivered late
old-generation callbacks. Resolve those and the #195 checkpoint boundary alongside
the next #196 stage instrumentation. Bun native transport work in #197 is deferred
by operator choice. #198 and retained-history/server-selection gates remain open;
the server and JavaScript client pins were not upgraded by these runs.

## Fixture and workload prerequisites

The local `wax-probe` manifest declares WAX blocks **437200000–437201999** with SHIP v0 and
block/traces/deltas enabled. The 49,863,282-byte compressed shard was SHA-256 checked as
`c1397aadae2b80459e5917dc05fb4dd825e39f4ee8f9a70b1735b048d0acfb9c`.
Streaming parsing found 2,000 complete, contiguous frame records and no partial tail.
This validates file/framing coverage, not decoded canonical block IDs or historical ABI correctness.
The older report's `wax-dense-190m` fixture was not present in the inspected workspace.
No large mainnet replay was run for this document.

Before a throughput baseline, pin and verify:
- Source/build/runtime/dependency and container-image digests; exact protocol/status bytes.
- Every shard digest, count, byte size, record coverage and decoded block/hash continuity.
- ABI history valid at the starting block and each deployment transition, plus required initial state.
- Expected logical output/count/order/coverage and durable progress, independently checked.
- Account/contract inclusion and exclusions, data density, cardinality and query distributions.

Fetching current ABIs and labeling them with a historical starting block is not an acceptable
seed. Use verified historical coverage or a synthetic genesis workload with known deployments.
Promote audited replay/seed tooling before relying on local untracked or ignored utilities.

Exercise synthetic correctness cases and representative heterogeneous real ranges. Run
unpaced backfill, paced live, and simultaneous live/backfill/query load. Include cold and warm
ABI/cache states, high cardinality, pagination/fan-out, ABI changes, poison-contract isolation,
transient/permanent storage failures, reconnect/restart and fork replacement. Add the complete
AtomicAssets/AtomicMarket corpus and selected Wire testnet as those components become executable.

## Experimental protocol

1. Validate fixture identity and the correctness oracle before timing. Save failed/error work;
   it cannot disappear from throughput denominators or be counted as successfully indexed.
2. Run in a dedicated throwaway project with explicit test-only ports, namespaces and volumes.
   Refuse unowned targets before any reset/purge. Record actual service versions/settings and
   destinations; configuration labels do not establish which server received requests.
3. Keep runtime, ES version, client version, transport, settings and source changes as separate axes.
   The current ES helper selects HttpConnection on Bun. Compare Node/Bun with the same supported
   transport to isolate runtime effects, and also report each runtime's supported end-to-end default.
4. Separate cold start/warmup from steady-state windows and end-to-end freshness. Measure spans
   within one process's clock domain; correlate queue hops without subtracting unrelated monotonic epochs.
5. Repeat at least five runs for initial comparisons, alternating controls where feasible. Retain
   variance, sample sizes and raw results; extend the run where tails or the claimed effect are unresolved.
6. Collect CPU/flamegraphs, allocation/heap/GC and OS/native I/O profiles for every contributing role,
   together with stage distributions and backend/queue time series. Compare profiler-on/off overhead.
   Do not infer a bottleneck merely from a long awaited span or a busy monitoring summary.
7. Optimize one measured limiting resource, rerun correctness and the relevant paired workloads,
   then publish benefit, regressions, resource cost and applicability limits. Preserve live priority,
   shared-reader backpressure and contract failure isolation.

Each machine-readable run manifest identifies the source commits and dirty diff digest; runtime
binary/version/flags; dependencies; OS/CPU/memory/limits; database and broker configuration;
fixture/ABI/output digests; workload/concurrency and warmup/window; collector versions/sampling
and overhead; success/failure/coverage; artifact hashes and result locations.
Missing stages and unfinished plugins remain visible in the ledger.

## Elasticsearch release review

Use the [9.5.5 source review](../research/elasticsearch-9.5.5.md) to select correctness,
memory/timeout and pressure regressions. It is a candidate matrix, not an upgrade decision.
Resolve the retained-index creation versions and supported server/client pairs through P02
before selecting an upgrade path. Preserve existing Elasticsearch history without silently
introducing a full Hyperion history reindex requirement.

Review official release notes and linked security advisories daily. The thread review is
scheduled for **09:00 America/Sao_Paulo**. Record what changed, source links, applicable Hyperion
paths, the test or profiling hypothesis, and the measured action. Monitoring does not upgrade
servers or dependencies. Separate a relevant upstream fix from a demonstrated Hyperion gain.

## Delivery slices

Track the coverage program in [tools #28](https://github.com/eosrio/hyperion-tools/issues/28):

| Slice | Deliverable | Blocked by |
|---|---|---|
| [History API #195](https://github.com/eosrio/hyperion-history-api/issues/195) | One guarded, verified fixture replay with an output oracle | None; use a known synthetic history if historical ABI coverage is unavailable |
| [History API #196](https://github.com/eosrio/hyperion-history-api/issues/196) | One reader-to-ack profile with CPU/wait/resource evidence | #195 |
| [History API #197](https://github.com/eosrio/hyperion-history-api/issues/197) | Bun 1.4.3 native transport and worker prerequisite decision | None for feasibility; full-stack replay follows the guarded harness |
| [History API #198](https://github.com/eosrio/hyperion-history-api/issues/198) | ES 9.5.5 sorted-search, pressure and compatibility candidate evidence | None for candidate fixtures; release selection remains in P02 |
| [History API #199](https://github.com/eosrio/hyperion-history-api/issues/199) | Correct configured ingest destinations before multi-node attribution | None |

The linked release issues own executable follow-through: guarded fixtures/harness, per-stage
observability and baseline artifacts, Bun runtime qualification, and ES version/client qualification.
A source-confirmed ingest-node routing defect must be reproduced and resolved before attributing
multi-node scaling to configured ingest targets. Keep these slices linked to the release tracker and
existing resilience/upgrade/domain issues. This workstream does not replace mandatory 4.5 parity,
recovery, protocol or retained-history acceptance.
