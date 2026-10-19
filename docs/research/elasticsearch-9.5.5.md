# Elasticsearch 9.5.5 review for Hyperion 4.5

Reviewed 2026-10-18. This is a source review and a qualification plan. It does not establish the version of any running cluster or authorize an upgrade.

The [9.5.5 release notes](https://www.elastic.co/docs/release-notes/elasticsearch#elasticsearch-9.5.5-release-notes) identify security fixes and changes affecting distributed search, refresh and mappings. Treat 9.5.5 as an explicit qualification candidate. Prefer measured improvements and correctness regressions tied to Hyperion's actual calls over a general expectation that a newer server will be faster.

## Observed Hyperion baseline

Source inspected: `eosrio/hyperion-history-api` at [`b571c5fb2e7c1422aa5bff31c45c1c7ac54caddc`](https://github.com/eosrio/hyperion-history-api/tree/b571c5fb2e7c1422aa5bff31c45c1c7ac54caddc). These are implementation observations, not live infrastructure measurements.

| Area | Evidence and implication |
| --- | --- |
| Client | [`package.json`](https://github.com/eosrio/hyperion-history-api/blob/b571c5fb2e7c1422aa5bff31c45c1c7ac54caddc/package.json#L29) pins `@elastic/elasticsearch` **8.19.1**. A dependency version does not identify the server version. |
| Transport | [`esConnectionOptions`](https://github.com/eosrio/hyperion-history-api/blob/b571c5fb2e7c1422aa5bff31c45c1c7ac54caddc/src/indexer/helpers/es-connection.ts#L25) selects `HttpConnection` on Bun, with an explicit environment override and `maxSockets: 256`. All seven `new Client` construction sites found under `src` spread this helper. Qualify the actual selected transport on Bun 1.4.3; the [older Bun note](https://github.com/eosrio/hyperion-history-api/blob/b571c5fb2e7c1422aa5bff31c45c1c7ac54caddc/docs/bun-elasticsearch.md) describes Bun 1.3.13 and a different client version. |
| Templates | [`index-templates.ts`](https://github.com/eosrio/hyperion-history-api/blob/b571c5fb2e7c1422aa5bff31c45c1c7ac54caddc/src/indexer/definitions/index-templates.ts#L4) sets `refresh_interval: 1s`, `best_compression`, and an action index sort on `global_sequence desc`. Its action fields include indexed keywords. It does not enable columnar mode or disable sequence numbers. Read effective settings of existing indices; a template is not proof of their settings. |
| Queries | [`get_actions`](https://github.com/eosrio/hyperion-history-api/blob/b571c5fb2e7c1422aa5bff31c45c1c7ac54caddc/src/api/routes/v2-history/get_actions/get_actions.ts#L57) sends sorted, offset-based searches with a default `10s` timeout. Its [default sort](https://github.com/eosrio/hyperion-history-api/blob/b571c5fb2e7c1422aa5bff31c45c1c7ac54caddc/src/api/routes/v2-history/get_actions/functions.ts#L3) is `global_sequence`. No active PIT, `search_after`, ES|QL or `multi_terms` call was found in the bounded `src` search. Plugins and external consumers were not exhaustively audited. |
| Version handling | [API startup](https://github.com/eosrio/hyperion-history-api/blob/b571c5fb2e7c1422aa5bff31c45c1c7ac54caddc/src/api/server.ts#L321) and [indexer startup](https://github.com/eosrio/hyperion-history-api/blob/b571c5fb2e7c1422aa5bff31c45c1c7ac54caddc/src/indexer/modules/master.ts#L1654) fetch and log the server version. The [health route](https://github.com/eosrio/hyperion-history-api/blob/b571c5fb2e7c1422aa5bff31c45c1c7ac54caddc/src/api/routes/v2/health/health.ts#L168) selects the newer health API for ES 8.12+. No supported-version range check was found in these paths or the bounded `src` search. API selection and connectivity are not compatibility validation. |
| Profiling | The [`db_indexing` span](https://github.com/eosrio/hyperion-history-api/blob/b571c5fb2e7c1422aa5bff31c45c1c7ac54caddc/src/indexer/workers/indexer.ts#L49) covers route completion. [`WorkerProfiler`](https://github.com/eosrio/hyperion-history-api/blob/b571c5fb2e7c1422aa5bff31c45c1c7ac54caddc/src/indexer/helpers/profiler.ts#L1) retains duration totals and counts, reporting every second. This does not provide latency distributions or isolate serialization, socket wait, server execution, retries and acknowledgment. |

One configuration defect can invalidate scaling experiments: [`prepareIngestClients`](https://github.com/eosrio/hyperion-history-api/blob/b571c5fb2e7c1422aa5bff31c45c1c7ac54caddc/src/indexer/connections/manager.class.ts#L140) loops over `ingest_nodes` but constructs every client using `_es.host`, ignoring the loop's `node`. Verify actual request destinations before attributing a result to multiple configured ingest nodes. This review changes no runtime code.

## Follow-up review: 2026-10-18 12:03 UTC

The [release notes](https://www.elastic.co/docs/release-notes/elasticsearch) still list
9.5.5 first. The [known-issues page](https://www.elastic.co/docs/release-notes/elasticsearch/known-issues)
still lists the previously recorded 9.5.0–9.5.2 indexing-pressure defect and its 9.5.3 fix;
no 9.5.5-specific entry was observed. The
[official security index](https://discuss.elastic.co/c/announcements/security-announcements/31)
shows October 6 as its latest advisory activity. All six linked advisories retain the recorded
affected versions, prerequisites and fixed releases. Their official Discourse first-post records
remain version 1, last updated October 6, before the previous review. No new actionable upstream
change was identified in this scope. This is not a complete cluster exposure assessment.

The repository pin audit adds the following baseline detail. These are tracked defaults and
resolved dependencies, not running-server observations or a qualified 4.5 support matrix.

| Repository revision and scope | Exact observed version | Evidence |
| --- | --- | --- |
| History API `release/4.5`, `e821fdfa687471427b6248a68fc40cf2e6c36047`, JavaScript dependencies | Client **8.19.1**, transport **8.10.1** | [Manifest](https://github.com/eosrio/hyperion-history-api/blob/e821fdfa687471427b6248a68fc40cf2e6c36047/package.json), [lockfile](https://github.com/eosrio/hyperion-history-api/blob/e821fdfa687471427b6248a68fc40cf2e6c36047/package-lock.json) |
| Same revision, end-to-end server default | **9.3.1**; overridable via `ES_VERSION` or the harness configuration | [Compose](https://github.com/eosrio/hyperion-history-api/blob/e821fdfa687471427b6248a68fc40cf2e6c36047/tests/e2e/docker-compose.yml#L37), [ChainManager](https://github.com/eosrio/hyperion-history-api/blob/e821fdfa687471427b6248a68fc40cf2e6c36047/tests/e2e/lib/chain-manager.ts#L46) |
| Hyperion Tools `main`, `0f3865925a681e60f0a3b1eab8ef90c940a35720`, benchmark server default | Elasticsearch and Kibana **9.4.2**; overridable via `ELASTIC_VERSION` | [Compose](https://github.com/eosrio/hyperion-tools/blob/0f3865925a681e60f0a3b1eab8ef90c940a35720/bench/docker-compose.yml#L15), [example environment](https://github.com/eosrio/hyperion-tools/blob/0f3865925a681e60f0a3b1eab8ef90c940a35720/bench/.env.example#L16) |

Neither harness default exercises the 9.5.5 candidate. Both Compose configurations use version
tags rather than immutable image digests. [History API #198](https://github.com/eosrio/hyperion-history-api/issues/198)
must record explicit server/image-digest overrides and the resolved client/transport for each
qualification run; a default-harness pass cannot establish 9.5.5 compatibility. These older server
versions also lie within the affected ranges of
[ESA-2026-185](https://discuss.elastic.co/t/elasticsearch-8-19-23-9-4-8-9-5-5-security-update-esa-2026-185/390859)
when its ES|QL access prerequisite holds. Treat that as a conditional test-environment exposure
check, not a claim about Hyperion production. No dependency, image default, cluster setting or
deployment was changed. No new Hyperion performance or compatibility measurements were taken.

## Harness adoption: 2026-10-18

Following the operator's request, the Tools benchmark's [Compose defaults](../../bench/docker-compose.yml)
and [example environment](../../bench/.env.example) now select Elasticsearch and Kibana **9.5.5**.
`ELASTIC_VERSION` remains an explicit override; existing local `.env` selections are not changed.
The [setup guide](../../bench/README.md) keeps the ES 9.4.2 measurements identified as historical
and requires actual server-version and image-digest evidence for new comparisons. The earlier
pin audit above remains a record of the revisions inspected at that time.

This adoption selects a benchmark candidate only. It changes no mappings, live cluster settings,
JavaScript dependency versions or existing history. Compatibility and profiling results belong
to [History API #198](https://github.com/eosrio/hyperion-history-api/issues/198); the default alone
does not qualify that server/client combination.

### First executed compatibility/profiling slice

[History API #200](https://github.com/eosrio/hyperion-history-api/pull/200) updates the E2E
Compose default and `ChainManager` fallback to 9.5.5 while retaining explicit overrides.
Its [initial evidence report](https://github.com/eosrio/hyperion-history-api/blob/298db4241d3ff3b43582d60fd0f797fe52921675/docs/performance/elasticsearch-9.5.5-initial.md)
records three unprofiled and two profiled single-node synthetic executions using client 8.19.1
and transport 8.10.1. Each verified 2,500 acknowledged documents, complete helper-generated
query results and exact owned-index cleanup. Node 24.19.0 default/HttpConnection and Bun 1.4.3
HttpConnection passed that bounded request surface. CPU/heap artifacts, server statistics,
hot threads and exact source/image/executable hashes are retained with checksums.

Two draft runs failed shard allocation because of the local Docker filesystem's disk
watermark; their reports and the isolated fixture adjustment are preserved. Both runtimes
also returned bulk `took` values exceeding client wall time in some samples; those were
initially unresolved timing warnings. The clock-control follow-up below explains the
reproduced pattern while preserving the raw measurements. These observations do not establish
a runtime speed ranking, a server regression or full Hyperion compatibility. Neither entire
chain E2E indexing nor the existing Tools bulk benchmark was run by this client probe.
Timeout/late-shard failures, replicas/pressure, full mappings and retained-index upgrades
remain open in [#198](https://github.com/eosrio/hyperion-history-api/issues/198).

### Bulk clock-control follow-up: 2026-10-18

The [controlled report and checksummed evidence](https://github.com/eosrio/hyperion-history-api/blob/a6398097cd861393b89d5fffb1ac383339ce3bf2/docs/performance/elasticsearch-9.5.5-clock.md)
resolve the timing-source investigation for the reproduced short-bulk pattern. Six raw
HTTP probes each issued 100 one-document bulks against one dedicated digest-pinned ES
9.5.5 fixture. Windows Node 24.19.0 and Java 27 colocated inside the Linux ES container
reproduced overshoots with the default interval (3/100 and 4/100), none with an explicit
zero interval, and overshoots again after restoring 200 ms (9/100 and 8/100). All 600
items succeeded, each exact probe index was removed, and the owned container/volume were
removed after identity/label checks. The Elasticsearch JavaScript client was not used.

The actual server build's [bulk timer injection](https://github.com/elastic/elasticsearch/blob/48acba663b09e70b115dd879329141506c377890/server/src/main/java/org/elasticsearch/action/bulk/TransportBulkAction.java#L143)
uses a clock that [defaults to a 200 ms cached update interval](https://github.com/elastic/elasticsearch/blob/48acba663b09e70b115dd879329141506c377890/server/src/main/java/org/elasticsearch/threadpool/ThreadPool.java#L247-L252).
[Zero bypasses that cache](https://github.com/elastic/elasticsearch/blob/48acba663b09e70b115dd879329141506c377890/server/src/main/java/org/elasticsearch/threadpool/ThreadPool.java#L789-L794).
A short request spanning an update can therefore report about 200 ms even when its full
HTTP round trip is shorter; requests between updates can report zero. Scheduling can
delay ticks, so 200 ms is not a guaranteed error bound. Raw values/warnings remain intact.
Use client wall time for request latency and correlate server counters/profiles; do not
subtract short-bulk `took` to estimate precise network overhead. The setting is startup-only;
it was changed only by recreating this disposable fixture. This is not a production tuning
recommendation, evidence of an ES regression or a runtime speed ranking.

The configured ingest-node defect is separately fixed in
[API #201](https://github.com/eosrio/hyperion-history-api/pull/201): six controlled routing
regressions, whole-project typecheck/build, 134 unit tests and exact-head Node 22/24 CI
passed at `95bae91d69d2a00151ba0cdfb00063b9ba8f7855`. That PR remains pending merge at
this checkpoint. Real TLS handshakes and sustained multi-node load still require their
own qualification. The clock diagnostic does not close the broader #198 acceptance.

## Relevant changes and qualification cases

The relevance judgments below are inferences from the cited upstream implementation and the pinned Hyperion source. None is a measured Hyperion improvement.

| Change | Scope supported by the upstream source | Hyperion qualification |
| --- | --- | --- |
| Sorted shard reduction | [#160132](https://github.com/elastic/elasticsearch/pull/160132) fixes reduction when a timed-out shard returns an empty result with incompatible sort metadata; it also handles older data-node responses during rolling upgrades. | **High priority:** sorted action queries spanning populated, empty and timing-out shards; both sort directions; explicit partial-result settings. Verify deterministic ordering, timeout/partial signaling and absence of reduction exceptions. Test mixed-version topology only where part of the operator's supported upgrade path. |
| Search memory release | [#160439](https://github.com/elastic/elasticsearch/pull/160439) releases shard results arriving after a search phase has failed and addresses associated request-breaker accounting. | **High priority:** repeated timeouts, cancellation, failed phases and late shard replies under concurrent reads/writes. Measure retained heap, breaker accounting and recovery after the workload ends. Do not equate this server fix with Hyperion process RSS or a faster successful query. |
| High-cardinality keyword routing | [#158603](https://github.com/elastic/elasticsearch/pull/158603) replaces a faulty script fallback for fuzzy, wildcard and case-insensitive term queries on fields without an inverted index, including columnar binary doc values. | **Conditional:** only exercise this specific regression on matching non-indexed/columnar mappings. Hyperion's ordinary indexed keyword fields do not establish exposure. This is not evidence of a general high-cardinality keyword speedup or a reason to change index format. |
| Idle-shard refresh exception | [#159526](https://github.com/elastic/elasticsearch/pull/159526) handles a queued refresh when its shard closes, preventing a spurious uncaught error during deletion, relocation or shutdown. | **Conditional:** first query after idle plus relocation/shutdown. Record actual effective refresh and search-idle settings. The fix does not establish a better refresh interval or justify disabling refresh on a live partition. |
| PIT consistency | [#160179](https://github.com/elastic/elasticsearch/pull/160179) verifies that a PIT reader context belongs to its intended shard. [#159248](https://github.com/elastic/elasticsearch/pull/159248) handles newer PIT encodings in mixed-version cross-cluster batched queries. | **Future/conditional:** when PIT pagination or cross-cluster operation is introduced, cover malformed/expired IDs, authorization, cancellation, context cleanup and supported mixed versions. These are not current `get_actions` performance gains. |
| Replica divergence with sequence numbers disabled | [#158870](https://github.com/elastic/elasticsearch/pull/158870) fixes replica divergence for this specialized index configuration. | **Conditional:** inspect effective settings and operator-created templates before marking applicable. Do not attribute this configuration to Hyperion's standard templates. |

An earlier 9.5 regression matters to the write qualification: Elastic's [known-issues page](https://www.elastic.co/docs/release-notes/elasticsearch/known-issues) describes primary/replica divergence when crossing indexing pressure limits in 9.5.0–9.5.2, fixed in 9.5.3 by [#158235](https://github.com/elastic/elasticsearch/pull/158235). Test pressure and recovery with replicas, item-level bulk outcomes, translog growth and post-recovery document identity. A benchmark must not count submitted or intermittently searchable, unacknowledged items as durable writes. No 9.5.5-specific entry appeared on that page at review time; this is not evidence that it has no defects.

## Security review scope

Review cluster exposure and roles against the actual advisories, separately from throughput. The following advisories identify 9.5.5 as fixed; they describe different prerequisites. The absence of a direct Hyperion call does not establish that another user, plugin or service cannot reach a feature.

| Advisory | Applicability to establish |
| --- | --- |
| [ESA-2026-199 / CVE-2026-103009](https://discuss.elastic.co/t/elasticsearch-8-19-23-9-4-8-9-5-5-security-update-esa-2026-199/390874) | Cross-cluster authorization bypass under RCS 2.0 on a fulfilling cluster. The advisory excludes deployments without the remote transport exposure and says it is not exploitable through the REST API. |
| [ESA-2026-198 / CVE-2026-103008](https://discuss.elastic.co/t/elasticsearch-8-19-23-9-4-8-9-5-5-security-update-esa-2026-198/390872) | Unbounded geometry recursion through scripted runtime fields. Establish who can submit such queries. |
| [ESA-2026-195 / CVE-2026-103005](https://discuss.elastic.co/t/elasticsearch-8-19-23-9-4-8-9-5-5-security-update-esa-2026-195/390869) | Connector descriptions can exhaust heap when connector management is exposed to an authorized user. |
| [ESA-2026-190 / CVE-2026-102409](https://discuss.elastic.co/t/elasticsearch-9-4-8-9-5-5-security-update-esa-2026-190/390863) | ES|QL text chunking recursion can terminate a node. Applicable feature introduced in 9.2; 8.x and 9.0–9.1 are excluded by the advisory. |
| [ESA-2026-189 / CVE-2026-102408](https://discuss.elastic.co/t/elasticsearch-8-19-22-9-4-8-and-9-5-5-security-update-esa-2026-189/390862) | Caller-supplied separators in ES|QL `CHUNK` can consume excessive CPU through regex backtracking. |
| [ESA-2026-185 / CVE-2026-102404](https://discuss.elastic.co/t/elasticsearch-8-19-23-9-4-8-9-5-5-security-update-esa-2026-185/390859) | ES|QL queries can cause uncontrolled memory growth. |

This is a scoped review, not a complete security inventory. Recheck the [official security announcements](https://discuss.elastic.co/c/announcements/security-announcements/31) for superseding fixes and operator-specific exposure before a rollout.

## Client and upgrade constraints

The [8.19.1 client source](https://github.com/elastic/elasticsearch-js/blob/v8.19.1/src/client.ts#L388) supplies `compatible-with=8` media types by default. Elastic's [REST compatibility documentation](https://www.elastic.co/docs/reference/elasticsearch/rest-apis/compatibility) supports the previous major as an upgrade bridge, explicitly without guaranteeing identical behavior. Its [JavaScript matrix](https://www.elastic.co/docs/reference/elasticsearch/clients/javascript/installation#compatibility-matrix) pairs client and server majors; a newer minor server also does not make new APIs appear in an older client.

Consequently, qualify **8.19.1 client + 9.5.5 server** as an explicit transitional combination if retained, including headers, deprecations, serialization and every used API. Evaluate a pinned 9.x client in a separate change with its own breaking-change and transport checks. Neither a successful connection nor TypeScript compilation qualifies either combination. Keep Node and Bun 1.4.3 results separate, recording exact client, transport, runtime and server versions.

For an 8.x server moving to 9.5.5, Elastic's [upgrade preparation](https://www.elastic.co/docs/deploy-manage/upgrade/prepare-to-upgrade) requires starting from 8.19.x and recommends its latest patch and Upgrade Assistant. Its [upgrade plan](https://www.elastic.co/docs/deploy-manage/upgrade/plan-upgrade) also calls out indices created in 7.x or earlier. Inventory actual index creation versions, snapshots, restore viability, deprecations and mixed-version constraints before choosing a path. The Hyperion 4.5 requirement to preserve existing history does not by itself authorize a server-major upgrade or make incompatible indices readable. Resolve a conflicting ES migration requirement explicitly; do not silently require a full Hyperion history reindex.

## Profiling and release review follow-through

Every ES qualification run should capture the effective cluster/index configuration and a reproducible workload: versions and commits; shard and replica layout; documents and bytes; mappings and sorts; storage and cache state; refresh policy; request concurrency; bulk size; and the actual receiving node. Compare live-only, backfill-only and concurrent live/backfill/query operation, preserving live priority and shared-reader backpressure.

Instrument the boundaries around [`bulkAction` and its settlement path](https://github.com/eosrio/hyperion-history-api/blob/b571c5fb2e7c1422aa5bff31c45c1c7ac54caddc/src/indexer/helpers/elastic-routes.ts#L377): local queue wait, serialization/bytes, connection wait, request wall time, server `took`, item failures/retries, acknowledgment, refresh visibility and durable progress. Record p50/p95/p99 and failed/timed-out work separately. Correlate with server CPU, heap/GC, breakers, indexing pressure, thread-pool queues/rejections, refresh, merges, disk and shard fan-out. A long client span alone cannot identify which resource is limiting throughput.

For each subsequent release-note review, record source links, newly relevant changes, exposure or performance hypothesis, qualification cases, measured evidence and a proposed action. Keep server patches, client changes, runtime changes and template changes separable so comparisons can attribute a result. Update tested compatibility only after qualification; release monitoring does not install or upgrade anything.
