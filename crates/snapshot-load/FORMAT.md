# Antelope Portable Snapshot `.bin` Format

This document describes the Antelope portable snapshot container used by Leap and
Spring/Savanna chains, and the portions `snapshot-load` reads directly from disk.
It is source-cited against Antelope Spring commit
`e6a99f68b67abc4d89fe716755b2e1394a4991f7`
(`chain_snapshot_header::current_version = 8`) and validated against
representative v2, v6, and v8 snapshots.

`snapshot-load` uses this format to decode active contract-table state without
starting nodeos or replaying SHiP. It emits Hyperion-shaped documents for
selected sync targets, either as NDJSON or through the MongoDB sink.

---

## 1. Scope

Read `chain_snapshot_header.version` from the first snapshot section and branch
on it. Do not infer the layout from a chain name.

| `chain_snapshot_header.version` | Era | Contract-table layout | `snapshot-load` support |
|---|---|---|---|
| 2-6 | Leap 1.x-5.x, pre-Savanna | single commingled `contract_tables` section | supported |
| 7 | Spring 1.0.0 transient format | split per-table sections | rejected, matching Spring 1.0.1+ |
| 8 | Spring 1.0.1+ / Savanna | split per-table sections | supported |

The outer file-format version at offset 4 is a separate constant (`1`). It is
not the chain snapshot version and must not drive contract-table parsing.

---

## 2. Container Format

All fixed-width integer fields are little-endian. Variable-length counts and
byte lengths use `fc::unsigned_int`: a 32-bit LEB128 varuint, 7 data bits per
byte and the high bit as continuation (`libfc/include/fc/io/raw.hpp:225-233`).

### 2.1 File Header

```text
offset 0: u32 LE  magic_number              = 0x30510550
offset 4: u32 LE  current_snapshot_version  = 1
offset 8: first section begins
```

- `magic_number = 0x30510550` (`snapshot.hpp:349`).
- `current_snapshot_version = 1` (`snapshot.hpp:16`). The writer emits it
  immediately after the magic (`snapshot.cpp:138-151`), and the reader asserts
  that it equals `1` (`snapshot.cpp:260-274`).
- The chain version (`2` through `8`) is the payload of the first section,
  `eosio::chain::chain_snapshot_header`.

### 2.2 Section Framing

Each section is written by `write_start_section` / `write_end_section`
(`snapshot.cpp:153-194`):

```text
section start S:
  S+0       : u64 LE  section_size
  S+8       : u64 LE  row_count
  S+16      : name bytes, not length-prefixed
  S+16+len  : 0x00 NUL terminator
  S+17+len  : payload = row_count rows, each fc::raw::pack-ed
```

`section_size` excludes its own 8-byte field. It counts:

```text
row_count(8) + section_name + NUL + payload
```

The next section starts at:

```text
next_section_offset = S + 8 + section_size
```

Section names are raw bytes plus the NUL terminator. Most are demangled C++ type
names such as `eosio::chain::account_object`; `block_state` and secondary-index
sections use fixed literals.

The file ends with a `u64` end marker value of `0xFFFFFFFFFFFFFFFF` in the place
where the next `section_size` would appear (`snapshot.cpp:196-201`).

### 2.3 Seeking a Section

The native reader `istream_snapshot_reader::set_section` rescans from offset 8
for each request (`snapshot.cpp:295-336`). A direct reader can therefore skip
unneeded sections by size and decode only the sections it needs.

```text
scan_pos = 8
loop:
    seek scan_pos
    read u64 section_size
    if section_size == 0xFFFFFFFFFFFFFFFF: stop
    next_section_pos = current_offset_after_size + section_size
    read u64 row_count
    read section name through the NUL terminator
    if name matches target: payload starts here
    else: scan_pos = next_section_pos
```

Name matching is exact, including the trailing NUL, so prefixes do not
false-match.

### 2.4 Section Order

Writer order comes from `add_to_snapshot` (`controller.cpp:2314-2341`):

1. `eosio::chain::chain_snapshot_header`
   - one row
   - payload is `u32 version` (`chain_snapshot.hpp:54`)
2. `eosio::chain::block_state`
   - one row
   - payload type is version-gated on load (`controller.cpp:2403-2424`)
3. controller index sections, skipping `database_header_object`
   (`controller.cpp:55-68`)
   - `account_object`
   - `account_metadata_object`
   - `account_ram_correction_object`
   - `global_property_object`
   - `protocol_state_object`
   - `dynamic_global_property_object`
   - `block_summary_object`
   - `transaction_object`
   - `generated_transaction_object`
   - `table_id_object`
   - `code_object`
4. contract rows
   - versions `< 7`: one literal `contract_tables` section
   - versions `>= 7`: one section per contract index type
5. authorization sections
   - `permission_object`
   - `permission_link_object`
6. resource-limit sections
   - `resource_limits_object`
   - `resource_usage_object`
   - `resource_limits_state_object`
   - `resource_limits_config_object`

For snapshots older than v7, `table_id_object` rows are inlined inside
`contract_tables`; the standalone `table_id_object` section is skipped on load
(`controller.cpp:2444-2452`). `genesis_state` is present only in v2 snapshots.

### 2.5 Header Example

A representative v6 snapshot starts as follows:

```text
magic    = 0x30510550   (50 05 51 30)
offset4  = 1            (01 00 00 00)
sec1 @8  : size=48  row_count=1
           name="eosio::chain::chain_snapshot_header"
           payload u32 = 6
           next = (8 + 8) + 48 = 64
sec2 @64 : block_state
```

The container version is `1`; the chain snapshot version is `6`, so the reader
uses the pre-v7 `contract_tables` path.

---

## 3. Contract Tables Before v7

For `chain_snapshot_header.version < 7`, all contract rows live in one section
named `contract_tables`. The native consumer is
`read_contract_tables_from_preV7_snapshot` (`controller.cpp:2220-2249`).

### 3.1 Per-Table Framing

```text
repeat until section boundary:
    table_id_object row
    for each index type in this order:
        key_value
        index64
        index128
        index256
        index_double
        index_long_double
            varuint count
            count rows of that index type
```

The `table_id` is not stored on each contract row. The reader reconstructs it
from the preceding `table_id_object` (`controller.cpp:2242`).

### 3.2 `table_id_object`

`FC_REFLECT(table_id_object,(code)(scope)(table)(payer)(count))`
(`contract_table_objects.hpp:335`). The chainbase object id is not serialized.

```text
code   : u64 LE   Antelope name
scope  : u64 LE   Antelope name
table  : u64 LE   Antelope name
payer  : u64 LE   Antelope name
count  : u32 LE
```

The serialized row is 36 bytes. `count` is the total number of rows across all
six index groups for that table, not only primary-key rows. For primary-only
tables such as `voters` and standard token `accounts`, this is equivalent to
the number of `key_value` rows.

### 3.3 `key_value` Rows

`key_value` rows are serialized through `snapshot_key_value_object`
(`database_utils.hpp:107-138`):

```text
primary_key : u64 LE
payer       : u64 LE   Antelope name
value_len   : varuint
value       : value_len raw bytes
```

`value` is the ABI-serialized contract row. It is byte-identical to SHiP
`contract_row_v0.value`, because nodeos stores the buffer provided to
`db_store_i64` unchanged (`apply_context.cpp:796-801`).

### 3.4 Secondary-Index Rows

Secondary-index rows are fixed-size. All five serialize
`primary_key`, `payer`, and `secondary_key`
(`contract_table_objects.hpp:338-345`).

| Index | secondary key encoding | row bytes |
|---|---|---|
| `index64` | `uint64` | 24 |
| `index128` | `uint128` | 32 |
| `index256` | two contiguous `uint128` values | 48 |
| `index_double` | IEEE `double` LE | 24 |
| `index_long_double` | 16-byte `uint128` LE | 32 |

For target tables decoded through primary keys, secondary rows only need to be
skipped correctly so the stream remains aligned.

---

## 4. Contract Tables in v7+

For `chain_snapshot_header.version >= 7`, contract rows are split into one
section per index type:

```text
eosio::chain::table_id_object
eosio::chain::key_value_object
eosio::chain::index64_object
eosio::chain::index128_object
eosio::chain::index256_object
eosio::chain::index_double_object
eosio::chain::index_long_double_object
```

The writer is `add_contract_rows_to_snapshot` (`controller.cpp:2185`); the
reader path is `read_contract_rows_from_V7plus_snapshot`
(`controller.cpp:2252`).

`eosio::chain::table_id_object` contains `row_count` 36-byte table rows. The
0-based row index is the flattened `t_id` used by the row sections
(`controller.cpp:2198-2202`).

Each contract-row section payload is:

```text
repeat until section boundary:
    t_id  : int64 LE
    count : varuint
    rows  : count rows of that index type
```

The row encoding itself is identical to pre-v7. To read contract table state,
parse `table_id_object` into `t_id -> (code, scope, table)`, then walk
`key_value_object` and join each group by `t_id`.

---

## 5. ABI Extraction

### 5.1 `account_object`

Contract ABIs live in the `eosio::chain::account_object` section, not in
`account_metadata_object`.

`FC_REFLECT(account_object,(name)(creation_date)(abi))`
(`account_object.hpp:107`):

```text
name          : u64 LE
creation_date : u32 LE
abi           : varuint length + length raw bytes
```

The metadata index only carries sequence/hash metadata (`account_object.hpp:47-65`).

### 5.2 ABI Blob Encoding

There are two byte views:

- In memory, the `account_object.abi` payload is a bare fc-packed `abi_def`.
  It has no outer blob length (`account_object.hpp:18-25`).
- In the serialized snapshot row, the `abi` field is a `shared_cow_string`
  with an outer `varuint32 length` followed by the payload bytes
  (`database_utils.hpp:279-298`).

When reading an ABI from a snapshot row, strip the outer field length first,
then pass the remaining payload to `AbiHandle::from_bin`.

```rust
let handle = AbiHandle::from_bin(payload_after_outer_length)?;
```

After the outer field length is removed, the bare `abi_def` begins with its
`version` string field. A typical ABI v1.2 payload starts with
`0e 65 6f 73 69 6f 3a 3a 61 62 69 2f 31 2e 32`: `varuint(14)` followed by
`eosio::abi/1.2`.

Empty ABI blobs have length 0 and should be skipped rather than passed to
abieos.

---

## 6. Decode Notes

For each selected `key_value` row, `snapshot-load` has:

```text
code, scope, table, primary_key, payer, value, snapshot_block
```

Hot-loop filtering is done on the `u64` Antelope names. Names are converted to
strings only when formatting the emitted document.

Common targets:

- `eosio:voters` -> Hyperion `IVoter`
- validated token-contract `accounts` -> Hyperion `IAccount`
- `eosio.msig:proposal` plus `approvals2` -> Hyperion `IProposal`
- arbitrary `code:scope:table` or `*` -> dynamic contract-state documents
- native permission sections -> Hyperion `IPermission`

Per-row ABI decode failures do not abort the run. The row is emitted with raw
hex fallback when the target shape supports it; framing/header errors remain
hard errors.

`block_num` is the snapshot head block. `snapshot-load` can derive it from
recognized provider filenames such as `snapshot-<64-hex block_id>.bin` or
trailing decimal snapshot names. For URL streams whose outer URL does not carry
the block id, the tar entry name is used when available. If no recognized name
is available, pass `--block-num`.

---

## 7. Reference Implementation Layout

Current `crates/snapshot-load` source layout:

```text
src/
├── main.rs     # CLI, input/decompression, version dispatch, pipeline wiring
├── reader.rs   # snapshot header/section framing, seek/skip, enumeration
├── tables.rs   # pre-v7 and v7+ contract-table walkers
├── model.rs    # raw rows, targets, ABI registry, token-contract validation
├── map.rs      # decoded rows -> Hyperion document shapes
├── mongo.rs    # high-throughput MongoDB sink
└── perms.rs    # native permission-section decode
abis/
└── transaction.abi.json
```

The scan is sequential because section framing and contract-table grouping are
length-prefixed. Decode work is parallel: the producer walks the snapshot,
workers own per-thread `AbiHandle` registries, and the writer drains either
NDJSON or MongoDB batches. Bounded channels provide backpressure so large
snapshots are not materialized in memory.

Representative CLI surface:

```text
--snapshot <path>
--snapshot-url <url>
--out <path>
--tables voters,accounts,...
--threads <N>
--block-num <N>
--inspect
--stats-only
--raw
--mongo <uri>
--tee <path>
```

---

## 8. Validation

`snapshot-load` validates the format with two complementary checks:

- byte-for-byte diffs against `spring-util snapshot to-json` for vanilla
  chains where Spring can reserialize the full state;
- in-tool structural invariants and ABI-decode coverage for forks with custom
  sections or protocol features that vanilla Spring can load but not
  reserialize.

Hard invariants:

- full-section consumption: each walker must stop exactly at the section
  boundary;
- pre-v7 count match: `table_id_object.count` equals the sum of all six index
  group counts;
- v7+ `t_id` order: `key_value_object` groups are strictly increasing.

Representative validation matrix:

| Chain | Net | Version | Layout | Validation | Result |
|---|---|---|---|---|---|
| Telos | mainnet | v6 | commingled | Spring `to-json` byte diff, 1,389,468 selected rows + invariants | byte-exact |
| Jungle4 | testnet | v8 | split | Spring `to-json` byte diff, 75,849 selected rows + invariants | byte-exact |
| Ultra | mainnet | v8 | split + custom sections | invariants + 100% ABI decode + Spring `info` | passed |
| FIO | mainnet | v2 | commingled + custom sections | invariants + 100% ABI decode + Spring `info` | passed |

For decoded output validation, compare at the snapshot head block against a
node restored from the same snapshot, a trusted API that still has the pinned
state, or a Hyperion instance indexed at the same block.

---

## 9. Known Limitations and Operational Notes

- Version 7 snapshots are rejected intentionally, matching Spring 1.0.1+.
- Contract-table decoding depends on the ABI active in the snapshot. Rows whose
  table is absent from the contract ABI may be emitted with raw hex fallback.
- Snapshot packaging varies by provider: bare `.bin`, `.bin.zst`, `.tar.gz`,
  and `.tar.zst` forms are common. Providers may rate-limit downloads, so CI and
  benchmarks should prefer cached fixtures.
- Large snapshots can be many gigabytes. The reader must stream sections and
  keep bounded in-flight row queues rather than buffering the full file.
