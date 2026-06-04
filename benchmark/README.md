# Benchmarks

Head-to-head benchmarks of hyperion-tools serving engines against the reference stacks they replace —
**throughput, memory, and byte-parity for one API surface at a time**. Each API gets its own
subfolder with a self-contained harness: Docker stacks for both sides, the snapshot/data fetch, the
load + serve scripts, a parity checker, and the results write-up.

| benchmark | API surface | our engine(s) | reference stack |
|---|---|---|---|
| [**lightapi/**](lightapi) | cc32d9 [`eosio_light_api`](https://github.com/cc32d9/eosio_light_api) — 16 HTTP + 4 WebSocket | [`light-api`](../crates/light-api) (MongoDB) and WormDB (mmap `.wseg` segment, via [`wseg-build`](../crates/wseg-build)) | cc32d9: nodeos + Chronicle + MariaDB + Perl |
| _atomicassets/_ | AtomicAssets API | _planned_ | _planned_ |

Start with **[`lightapi/`](lightapi)** for the parity methodology, the operator comparison, and the
WormDB throughput/memory matrices.

## Adding a benchmark

Create a sibling folder (e.g. `atomicassets/`) with the same shape as `lightapi/`:

```
benchmark/<api>/
  README.md             # what's compared + how to run
  docker-compose.yml    # both stacks, gated behind compose profiles
  ours/                 # our Dockerfile + config + entrypoints
  <reference>/          # the reference stack's images
  snapshot/             # one-shot data fetch
  scripts/              # parity-check + benchmark drivers
  COMPARISON.md         # operator-perspective write-up + results
```

Large local outputs (`*.wseg`, snapshots, generated account lists) are gitignored repo-wide for this
tree — see `.gitignore`.

> ES write-side (indexing throughput) benchmarks are a separate concern and live under
> [`../bench`](../bench).
