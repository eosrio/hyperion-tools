# wseg-build — frozen WormDB Light-API segment builder

Builds a **frozen, memory-mappable columnar segment** (`.wseg`) of the Light-API tables from the per-chain
Hyperion MongoDB. WormDB serves the Light-API reads from this file — an Antelope-name `u64` index plus blob
arenas — at **tens-of-MiB resident**, with no per-key heap overhead.

It is the static, read-optimized counterpart to the Mongo-backed [`light-api`](../light-api) server: same
data, served from one mmap'd file instead of a database.

## Tables

| id | table | blob payload |
|---|---|---|
| 0 | `balances` | holder → packed `"<contract>\t<symbol>\t<decimals>\t<amount>\n…"` (one line per token) |
| 5 | `accinfo`  | account → cc32d9 accinfo fragment (`resources…linkauth[,code]`) |

The `balances` table is built by streaming the Mongo `accounts` collection in `scope`-sorted (index)
order, so a holder's rows arrive contiguously and pack into a single blob per holder.

## Usage

```bash
# build both tables for WAX into wax.wseg (defaults shown)
wseg-build --mongo-uri mongodb://127.0.0.1:27017 --db hyperion_wax --chain wax --out wax.wseg

# build only the balances table
wseg-build --db hyperion_eos --chain eos --out eos.wseg --tables balances

# parity check: render one account's accinfo fragment and exit (no build)
wseg-build --db hyperion_wax --probe someaccount
```

| flag | default | meaning |
|---|---|---|
| `--mongo-uri` | `mongodb://127.0.0.1:27017` | source Mongo (`user:pass@` is redacted in logs) |
| `--db` | `hyperion_wax` | per-chain database to read |
| `--chain` | `wax` | chain name (metadata) |
| `--out` | `wax.wseg` | output segment path |
| `--tables` | `balances,accinfo` | comma-separated tables to build |
| `--probe <acct>` | — | render one account's accinfo fragment to stdout and exit |

## `.wseg` format

Little-endian throughout. Layout: a 40-byte header (`WSEG0001` magic + version + table count), a 48-byte
table directory entry per table, then — per table, in directory order — its sorted index region
(`key u64 | off u64 | len u32`, 20 bytes/entry, ascending by key) followed by its blob arena. The reader
lives in WormDB (`src/storage/segment.zig`); see `docs/WSEG_FORMAT.md` there for the authoritative spec.
