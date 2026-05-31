# Local benchmark stack — direct-from-disk indexer → Elasticsearch

A self-contained, **local** environment to load the output of the direct-from-disk indexer
(`action-proto` / `delta-proto`, and the `abi-scanner` ABI snapshot) into Elasticsearch with
Hyperion-compatible index mappings + `_id`/`_index` rules, and to **measure the ES write-side
throughput and storage** — the ceilings that ultimately govern backfill cost.

Templates are **composable index templates** (`_index_template`), so they work on **Elasticsearch
8.x and 9.x**. The action template is also **storage-tuned** (~−39% vs the stock Hyperion mapping —
see *Storage efficiency* below), and `logsdb` index mode is an opt-in for a bit more on ES ≥ 8.17/9.x.

> ⚠️ **LOCAL ONLY.** This is meant to run on a machine **you** control, against a **throwaway**
> Elasticsearch. The loader creates and fills indices and tunes index settings. **Never point it at
> a production ES cluster.** Both `apply-templates.sh` and `bulk-load.py` refuse any non-loopback ES
> host unless you set `BENCH_ALLOW_EXTERNAL_ES=1` (please don't).

It is **chain-agnostic.** WAX is our primary test chain, but the reader and this stack work for any
Antelope chain (EOS, Telos, UX, Proton, …) — set `CHAIN` and point the reader at that chain's
state-history. **Community test runs on other chains are very welcome** (see *Contributing*).

---

## Where this fits

```
  state-history logs (local disk)
        │   trace_history.{log,index}  +  blocks.{log,index}  +  abi-index NDJSON
        ▼
  action-proto / delta-proto   ──►  NDJSON (Hyperion <chain>-action / -delta doc shape)
        │
        ▼
  bulk-load.py  ──►  Elasticsearch `_bulk`  ──►  <chain>-action-v1-*, <chain>-delta-v1-*
                                                  (this stack: ES + Kibana)
```

The standalone NDJSON → `_bulk` path here isolates and measures the **ES write ceiling**. The
eventual full integration (reader → RabbitMQ → Hyperion ingestors → ES, master-controlled) reuses
the same ES + templates; the `--profile full` services below are scaffolding for that step.

---

## Prerequisites

- Docker + Docker Compose v2 (`docker compose`).
- Linux host tweak for Elasticsearch (one-time):
  ```bash
  sudo sysctl -w vm.max_map_count=262144      # add to /etc/sysctl.conf to persist
  ```
- Python 3 (stdlib only — no pip installs).
- A built reader binary (`action-proto` / `delta-proto`) and, for the chain under test:
  - its `state-history` dir (`trace_history.{log,index}` for actions),
  - its block log dir (`blocks.{log,index}`) for `@timestamp` + `producer`,
  - an **abi-index** NDJSON (produced by `abi-scanner`; for WAX, the published snapshot release).

## Quick start

```bash
cd bench
cp .env.example .env          # then edit CHAIN, ES_JAVA_OPTS, etc.

# 1. bring up Elasticsearch + Kibana
docker compose up -d
# wait for green/yellow:
curl -s localhost:9200/_cluster/health?wait_for_status=yellow\&timeout=60s

# 2. create the Hyperion-shaped index templates for your chain
./scripts/apply-templates.sh          # reads CHAIN/ES from .env

# 3. produce reader output (example: WAX actions for a small range)
#    (run from the abi-scanner repo root, against your local state-history)
action-proto \
  --from-disk /path/to/state-history \
  --blocks-dir /path/to/blocks \
  --abi-index  /path/to/abi-index.ndjson \
  --start 437400000 --end 437410000 --threads 8 \
  --out /tmp/actions.ndjson

# 4. load into ES and read the write throughput
python scripts/bulk-load.py --mode action /tmp/actions.ndjson
#   -> [bulk-load] 6.1M docs in 84.2s -> 72,000 docs/s | 4210.5 MB (50.0 MB/s) | ... | errors=0

# 5. verify
curl -s "localhost:9200/_cat/indices/${CHAIN:-wax}-action-*?v"
curl -s "localhost:9200/${CHAIN:-wax}-action-*/_count"
# open Kibana at http://localhost:5601 (Dev Tools / Discover) to browse the docs
```

Deltas are identical with `delta-proto` + `--mode delta`:
```bash
delta-proto --from-disk /path/to/state-history --abi-index /path/to/abi-index.ndjson \
  --start 437400000 --end 437410000 --threads 8 --out /tmp/deltas.ndjson
python scripts/bulk-load.py --mode delta /tmp/deltas.ndjson
```

## Other chains

Everything is keyed off `CHAIN`:
```bash
# .env: CHAIN=telos
./scripts/apply-templates.sh                 # creates telos-action / telos-delta / telos-abi templates
# run the reader against your Telos node's state-history, then:
python scripts/bulk-load.py --mode action --chain telos /tmp/telos-actions.ndjson
```
You need an abi-index for that chain — run `abi-scanner` against the chain once to build it (see the
main README), or reuse a published snapshot if one exists.

## Storage efficiency (field tuning + `logsdb`)

Storage is the dominant long-term cost of a full history index, so the **action template is tuned**
beyond Hyperion's stock mapping. Measured on a dense WAX range (412k action docs, 1 shard,
`best_compression`, force-merged):

| mapping | bytes/doc | vs stock Hyperion | ES |
|---|---|---|---|
| faithful (Hyperion stock) | 392 | — | 8.x / 9.x |
| **field-tuned (this template)** | **239** | **−39%** | 8.x / 9.x |
| **field-tuned + `logsdb`** | **224** | **−43%** | ≥ 8.17 / 9.x |

**Where the win comes from** (via ES `_disk_usage`): the biggest cost in the stock mapping is the
`doc_values` of high-cardinality hex/sequence fields that are never sorted or aggregated — `act_digest`
alone was ~27% of the index. The template therefore:

- `act_digest` → `index:false, doc_values:false` (kept in `_source`, still **retrievable**, just not searchable);
- `trx_id` → `doc_values:false` (still **searchable** for `get_transaction`, just not sortable/aggregatable);
- `receipts.{global_sequence,recv_sequence,auth_sequence.sequence}` → `doc_values:false`.

Everything the real queries need is preserved (sort by `global_sequence`, `act.account`/`act.name`,
`@transfer.*`, `receipts.receiver`, `trx_id` search). This is a **mapping** change — works on ES 8.x
and 9.x alike. *(The delta template is still the faithful Hyperion mapping; tuning it is a TODO once
measured.)*

**`logsdb` index mode** (ES ≥ 8.17 / 9.x): set `INDEX_MODE=logsdb` in `.env` (or env). It adds a few
more storage points and indexes slightly faster, and is **fully query-compatible** — `act.data` is
retained (ES stores `enabled:false` fields even under synthetic source) and the `global_sequence`
sort is kept. It's opt-in because it requires ES ≥ 8.17. *(Synthetic `_source`'s headline savings
don't apply here because `act.data`/`signatures` are `enabled:false`; that's why the field tuning,
not the mode, is the main lever.)*

Numbers are data-dependent (repetitive, sorted event data compresses better) — **measure on your
chain/range**; the loader prints `MB` and you can compare `_cat/indices?bytes=b&h=index,pri.store.size`.

## Write-benchmark tuning

The templates ship with Hyperion-style settings (`refresh_interval: 1s`, `number_of_replicas: 0`,
`best_compression`, `number_of_shards: 4` for action/delta) and the storage tuning above. For a
**catch-up / backfill** write benchmark, the textbook wins (Elastic's own guidance) are:

- **`INDEX_MODE=logsdb`** (ES ≥ 8.17/9.x) — smaller + slightly faster indexing; re-run `apply-templates.sh`.

- **Heap:** `ES_JAVA_OPTS=-Xmx16g -Xms16g` in `.env` (≤ ~50% RAM, ≤ 31g).
- **Disable refresh during load:** edit `templates/*.json` → `"refresh_interval": "-1"`, re-apply,
  load, then `curl -XPOST localhost:9200/<chain>-*/_refresh` and (optionally) restore `1s`.
- **Replicas already 0** (single node). Keep it.
- **Bigger bulk requests:** `--batch 8000` (and give the loader a beefier box if it becomes the
  limiter — watch the `docs/s` vs ES CPU).
- **Use the Rust loader, not Python, for the ceiling.** `bulk-load.py` is fine for quick checks but
  GIL-bound (~137k docs/s single-process). For the actual write ceiling, build + use the in-repo
  `es-load` (no GIL, parallel posters, same `_id`/`_index` rules):
  ```bash
  cargo build --release --bin es-load
  ./target/release/es-load --file actions.ndjson --es http://localhost:9200 \
    --mode action --chain wax --workers 32 --batch 4000
  ```
  Co-locate it with the ES under test (same box / LAN — NOT over a WAN link, which measures the
  network, not ES). Reference point: on a 32-core box, ES 9.4.2 (16g heap, logsdb, `refresh=-1`),
  `es-load` plateaus at **~384k docs/s / ~400 MB/s** at 32 workers (80% box CPU, 0 write rejections)
  vs ~328k for 6 Python processes. The reader decodes ~3–4× faster than that, so **ES `_bulk` is the
  system write ceiling** — exactly why the cold-tier metadata trimming matters.

Report the `docs/s` **and** whether ES or the loader is the bottleneck (ES CPU near 100% → ES-bound,
which is the number we care about). With the loader co-located it shares the box's CPU with ES;
ES on dedicated hardware (loader/reader on a separate LAN host) will go higher.

## Full write path (opt-in, for the ingestor integration)

```bash
docker compose --profile full up -d          # + RabbitMQ (:15672 mgmt UI) + Redis
```
This brings up the services the eventual reader → AMQP → Hyperion-ingestor → ES path will use. Not
needed for the standalone `_bulk` benchmark above.

## Teardown

```bash
docker compose --profile full down -v         # -v also drops the ES/Redis volumes
```

## Contributing / sharing results

We test mostly on WAX; **runs on other chains and other hardware are exactly what helps.** When you
share a result, please include: chain, block range + doc count, ES version + heap + node specs
(CPU/RAM/disk), `docs/s` and `MB/s`, whether ES or the loader was the bottleneck, and any mapping
errors. Open an issue with the `bench` label. PRs that improve the templates (keeping them faithful
to Hyperion's `index-templates.ts`), add a `--profile full` ingestor wiring, or harden the loader
are welcome.
