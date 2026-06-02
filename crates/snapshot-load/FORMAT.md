# Antelope (Spring/Leap) Portable Snapshot `.bin` — Authoritative Format Spec & `snapshot-load` Implementation Blueprint

**Status:** consolidated, source-cited. All byte-layout claims are verified against the Spring source
clone at `P:\eosrio\hyperion-tools\.spring-src` (branch `main`, commit `e6a99f68b67abc4d89fe716755b2e1394a4991f7`,
`chain_snapshot_header::current_version = 8`) and empirically re-checked against the real Telos snapshot at
`P:\eosrio\hyperion-tools\snapshots\telos-extract\snapshot-1c0bedeedf9a22b827d8f9b50f26c9310fdb6cb1e20177f99d979370334cbe70.bin`
(1,611,413,260 bytes, `chain_snapshot_header.version = 6`).

Where the initial findings and the adversarial verdicts disagreed, **the verdicts win**. One claim was
**refuted** (V4-abi "loadable as-is with no extra unwrapping") and is corrected in §4. Open questions
flagged `uncertain` are carried verbatim into §8 with the exact source location to resolve them.

---

## 1. Scope & decision

**Goal.** Read an Antelope portable snapshot `.bin` **directly off disk** (no nodeos, no SHiP replay) and
emit Hyperion-shaped NDJSON for selected contract tables. The two first targets:

- **`eosio` `voters`** → `voter_info` rows (governance / staking).
- **token-contract `accounts`** → balances. The `accounts` table exists under *any* token contract
  (`eosio.token`, plus every user token), keyed `scope = holder account`.

**Sink is deferred.** Phase 1 emits **NDJSON to stdout/file**; an Elasticsearch / Hyperion ingest sink is
a later concern. NDJSON lets us validate decode correctness in isolation (diff against `get_table_rows`).

**Versions to support.** Read `chain_snapshot_header.version` from the first section and branch on it:

| `chain_snapshot_header.version` | Era | Contract-table layout | Support? |
|---|---|---|---|
| 2–5 | Leap 1.8–3.0 | commingled `contract_tables` | not targeted (no live mainnet emits these) |
| **6** | Leap 3.1.x / 4.x / 5.x (pre-Savanna) | **single commingled `contract_tables` section** | **YES — Telos mainnet today, WAX** |
| 7 | Spring 1.0.0 | per-table split sections | rejected on load by current Spring; transient |
| **8** | Spring 1.0.1+ (Savanna) | **per-table split sections** | **YES — current EOS, Jungle4, soon Telos** |

Telos mainnet currently emits **v6**; it will switch to **v8** when Savanna activates. The reader **must
not assume Telos == 8** — it must read the header and branch. We support **6 and 8** (7 is a transient,
rejected by Spring 1.0.1+, so treat as an error). The outer **file-format version** (offset-4 u32) is a
separate constant `= 1` (see §2 and §8) — do **not** confuse it with the chain version.

---

## 2. Snapshot binary format — byte-exact

All integers are **little-endian** (the writer does raw `memcpy` of fixed-width fields:
`s.write((char*)&v, sizeof(v))`, `libfc/include/fc/io/raw.hpp:374-379`). Variable-length counts/lengths
are `fc::unsigned_int` = **LEB128 varuint** (7 data bits/byte, high bit = continuation;
`libfc/include/fc/io/raw.hpp:225-233`); the unpacker caps at 32 bits, so all counts/lengths are
effectively `u32`.

### 2.1 File header (8 bytes, nothing else before the first section)

```
offset 0:  u32 LE  magic_number             = 0x30510550   (bytes 50 05 51 30)
offset 4:  u32 LE  current_snapshot_version  = 1           (bytes 01 00 00 00)
offset 8:  first SECTION begins
```

- `magic_number = 0x30510550` — `snapshot.hpp:349`.
- `current_snapshot_version = 1` — `snapshot.hpp:16`. This is the **snapshot file/container format version**
  (the History comment: *"Version 1: initial version with string identified sections and rows"*). It has
  been `1` since inception and is written by the writer constructor immediately after the magic
  (`snapshot.cpp:138-151`). **It is NOT the chain version.** The reader hard-asserts it equals `1`
  (`snapshot.cpp:260-274`).
- The **chain snapshot version** (the "6" / "8" that selects section membership) is the **u32 payload of
  the first section** (`chain_snapshot_header.version`, see §2.4), *not* this field.

### 2.2 Section framing

Each section (writer: `write_start_section` `snapshot.cpp:153-170`, `write_end_section`
`snapshot.cpp:177-194`):

```
section start S:
  S+0       : u64 LE  section_size
  S+8       : u64 LE  row_count
  S+16      : name bytes (name.size() chars, NOT length-prefixed)
  S+16+len  : 0x00      (single NUL terminator)
  S+17+len  : payload = row_count rows, each fc::raw::pack-ed
```

- **`section_size` excludes its own 8-byte field.** The writer back-patches
  `section_size = restore - section_pos - sizeof(uint64_t)`, i.e. it counts
  `row_count(8) + name + NUL + payload`. (`snapshot.cpp:180`; reader comment confirms verbatim:
  *"section size does not include the section size record itself"*, `snapshot.cpp:514-518`.)
- The name is raw bytes + one `0x00`; **no length prefix**. Name = demangled C++ type
  `boost::core::demangle(typeid(T).name())` → `"eosio::chain::<TypeName>"` (`snapshot.hpp:19-24`), except
  `block_state` (explicit literal `"eosio::chain::block_state"`) and the secondary-index sections (fixed
  literals via `SNAPSHOT_SECONDARY_SECTION_NAME`, `contract_table_objects.hpp:315-321`).

### 2.3 Skipping / seeking a named section (no row decode required)

Reader `istream_snapshot_reader::set_section` (`snapshot.cpp:295-336`):

```
scan_pos = header_pos + 8          # 8 = sizeof(magic u32) + sizeof(version u32)
loop:
    seek scan_pos
    read u64 section_size
    if section_size == 0xFFFFFFFFFFFFFFFF:  # end-of-file marker
        stop (section not found -> error)
    next_section_pos = (pos just after the size u64) + section_size
    read u64 row_count
    compare name bytes against target, then require next byte == 0x00 (NUL)
    if match: num_rows = row_count; stream positioned at first row; return
    else: scan_pos = next_section_pos; continue
```

Key facts for an independent reader:
- **`next_section_offset = (offset just after the u64 size field) + section_size`** = `S + 8 + section_size`.
- The file ends with a `u64 == 0xFFFFFFFFFFFFFFFF` end marker where a `section_size` would be
  (`finalize()`, `snapshot.cpp:196-201`). Stop on it.
- `set_section` **rescans from `header_pos` on every call** and inspects no cross-section state, so a
  consumer may read **only** the sections it wants, **in any order**, skipping all others by size. (Verdict
  V6-seek: **confirmed**.)
- Name match is exact-string **plus** the trailing NUL, so a name that is a prefix of another never
  false-matches.

### 2.4 Ordered section list (writer order, `add_to_snapshot` `controller.cpp:2314-2341`)

1. `eosio::chain::chain_snapshot_header` — **1 row**, payload = `u32 version` (the chain version; `=6` on
   the Telos anchor). `FC_REFLECT(chain_snapshot_header,(version))`, `chain_snapshot.hpp:54`.
2. `eosio::chain::block_state` — 1 row. Payload type is version-gated on **read**: `>=8` → `snapshot_block_state_data_v8`;
   `==7` → throws (unsupported); `<=6` → legacy `snapshot_block_header_state_legacy_v2` (clamped v2 range)
   or `_legacy_v3` (v3..6). For the Telos v6 anchor: **legacy_v3** (`controller.cpp:2403-2424`). The
   **head block num** is inside this row (see §5 for sourcing block_num).
3. `controller_index_set` indices in declared order, **skipping `database_header_object`**
   (`controller.cpp:55-68`): `account_object`, `account_metadata_object`, `account_ram_correction_object`,
   `global_property_object`, `protocol_state_object`, `dynamic_global_property_object`,
   `block_summary_object`, `transaction_object`, `generated_transaction_object`, `table_id_object`,
   `code_object`.
   - **v6 caveat:** on load, the standalone `table_id_object` section is **skipped** for `version < 7`
     because `table_id` rows are inlined inside `contract_tables` (`controller.cpp:2444-2452`). On the
     write path of an old Leap v6 node it likewise was not a standalone section.
4. **Contract rows.**
   - **v6 (`< 7`):** ONE commingled section literally named **`contract_tables`** (§3).
   - **v7+:** one section per contract index type (`eosio::chain::key_value_object`,
     `eosio::chain::index64_object`, …). Reader: `read_contract_rows_from_V7plus_snapshot`.
   - Dispatch: `controller.cpp:2525-2530`.
5. `authorization` (`authorization_manager.cpp:108-122`): `permission_object`, `permission_link_object`
   (`permission_usage_object` is **skipped**, inlined).
6. `resource_limits` (`resource_limits.cpp:13-18`): `resource_limits_object`, `resource_usage_object`,
   `resource_limits_state_object`, `resource_limits_config_object`.

`genesis_state` is present only in v2 snapshots; not on v6/v8.

### 2.5 Finding our two targets

- **`account_object`** (for ABIs, §4): section name `"eosio::chain::account_object"` (demangled). Seek to
  it by §2.3.
- **Contract data** (for `voters` / `accounts` rows, §3): **v6** → section `"contract_tables"`; **v8** →
  iterate the per-table sections (`"eosio::chain::key_value_object"` for primary KV rows). Branch on the
  chain version read in step 1.

### 2.6 Worked example — matches the empirical anchor (verified on the local Telos `.bin`)

PowerShell re-parse of the first 96 bytes confirms exactly:

```
magic    = 0x30510550   (50 05 51 30)
offset4  = 1            (01 00 00 00)              -> current_snapshot_version (file format), NOT chain ver
sec1 @8  : size=48  row_count=1  name="eosio::chain::chain_snapshot_header"  NUL  payload u32 = 6
           48 = 8(row_count) + 36(name 35 chars + NUL) + 4(payload u32)
           next = (8+8) + 48 = 64
sec2 @64 : size=3326 row_count=1 name="eosio::chain::block_state"
```

So **file format version = 1**, **chain snapshot version = 6** → pre-V7 commingled `contract_tables` path.

---

## 3. `contract_tables` (v6) — byte-exact

For `chain_snapshot_header.version < 7`, all contract rows live in the single section `"contract_tables"`.
Authoritative consumer: `read_contract_tables_from_preV7_snapshot` (`controller.cpp:2220-2249`). The v6
*writer* is not in this Spring tree (it writes v8 split sections), but the v8 writer's per-table inner
framing corroborates the row encodings, and the reader is the canonical spec for the bytes old Leap wrote.

### 3.1 Per-table framing (flat row stream, repeated until the section is exhausted)

```
repeat until section empty {
    [ table_id_object row ]                       # see 3.2
    for each of the 6 index types IN THIS ORDER:  # 3.3 order
        [ unsigned_int(varuint) count ]
        [ count rows of that index type ]         # 3.4 / 3.5
}
```

The `table_id` is **NOT** stored on each contract row; the reader reconstructs it from the preceding
`table_id_object` (`row.t_id = t_id`, `controller.cpp:2242`). A `table_id_object` row is always present
per table even if some index has `count = 0`.

### 3.2 `table_id_object` row — fixed 36 bytes

`FC_REFLECT(table_id_object,(code)(scope)(table)(payer)(count))` (`contract_table_objects.hpp:335`). `id`
is **not** serialized.

```
code   : u64 LE   (name)
scope  : u64 LE   (name)
table  : u64 LE   (name)
payer  : u64 LE   (name)
count  : u32 LE   (plain uint32, NOT a varuint)
= 36 bytes
```

`name` is a `u64` wrapper (`FC_REFLECT(name,(value))`, `name.hpp:45,190`) → plain 8-byte LE.

> **`count` semantics (CORRECTED — verified in source + empirically).** `count` is the total number of
> rows across **all** of the table's indices — the primary `key_value` index **plus every secondary
> index** — *not* just the primary rows. Both `db_store_i64` and the secondary `generic_index::store` do
> `++t.count` (`apply_context.hpp:197`), and the table is erased when `count` hits 0. The correct
> consistency check is therefore **`count == Σ(row counts of all 6 index groups)`**. Empirically: 0
> mismatches across all 2,464,131 tables of the Telos v6 snapshot when summed this way; checking
> `count == primary-only` falsely mismatches the ~2.5% of tables that carry secondary indices. For
> `voters`/`accounts` (primary-index-only structs) this reduces to `count == primary rows`.

### 3.3 Index-type order (`contract_database_index_set`, `controller.cpp:70-77`)

`walk_indices` visits in declared order:

1. `key_value` (primary i64 table)
2. `index64`
3. `index128`
4. `index256`
5. `index_double`
6. `index_long_double`

### 3.4 `key_value` row — variable

Serialized via `snapshot_key_value_object` (`database_utils.hpp:107-138`), **not** the default reflection:

```
primary_key : u64 LE
payer       : u64 LE  (name)
value_len   : unsigned_int (LEB128 varuint, 1-5 bytes)
value       : value_len raw bytes  <- the ABI-serialized contract row
```

No `t_id`, no `id`, no trailing padding. **To skip:** read 16 bytes, read varuint, skip `value_len` bytes.

### 3.5 Secondary-index rows — fixed sizes (for skipping)

All five: `FC_REFLECT(type,(primary_key)(payer)(secondary_key))` (`REFLECT_SECONDARY`,
`contract_table_objects.hpp:338-345`). Prefix is always `primary_key u64 | payer u64`. `secondary_key`
encoding (`database_utils.hpp` operators; `std::array` packs as contiguous scalars,
`libfc/include/fc/io/raw.hpp:674-679`):

| Index | secondary_key | secondary_key bytes | total row bytes |
|---|---|---|---|
| `index64` | `uint64` | 8 | 24 |
| `index128` | `uint128` | 16 | 32 |
| `index256` | `std::array<uint128,2>` (contiguous, no length prefix) | 32 | 48 |
| `index_double` | `float64` stored as IEEE `double` LE | 8 | 24 |
| `index_long_double` | `float128` stored as 16-byte `uint128` LE (NOT 80-bit x87) | 16 | 32 |

No `t_id`/`id`/length anywhere. For our targets we only **decode** `key_value` rows; secondary rows just
need correct skip sizes to advance the cursor.

### 3.6 `value` == ABI-serialized row (confirmed)

`key_value_object.value` is exactly the buffer a contract handed to the `db_store_i64` WASM intrinsic
(`apply_context.cpp:796-801` does `o.value.assign(buffer, buffer_size)`; nodeos never transforms it). It is
byte-identical to SHiP's `contract_row_v0.value`. Therefore `AbiHandle::decode_table_row(table, value)`
decodes it **given the contract's matching ABI** and the table's declared row struct. (Verdict
V5-valueeq: **confirmed**, with the caveat that some system tables / secondary objects are not plain
ABI rows.)

---

### 3.7 v8 split-table sections — byte-exact (validated on Jungle4)

For `chain_snapshot_header.version >= 7` the single `contract_tables` section is replaced by **one section
per index type**: `eosio::chain::{table_id_object, key_value_object, index64_object, index128_object,
index256_object, index_double_object, index_long_double_object}`. (Writer: `add_contract_rows_to_snapshot`,
`controller.cpp:2185`; reader: `read_contract_rows_from_V7plus_snapshot`, `:2252`.)

- **`eosio::chain::table_id_object`** is now a standalone section: `row_count` rows, each the 36-byte
  `table_id_object` (code|scope|table|payer u64 | count u32) in by-id walk order. Its **0-based row index
  is the "flattened" `t_id`** the row sections reference (chainbase re-flattens ids on load,
  `controller.cpp:2198-2202`).
- Each **contract-row section** payload is a flat stream, repeated to the section boundary:
  `[t_id: int64 LE (8 bytes)] [count: varuint] [count rows]`. `t_id` is `fc::raw::pack(oid._id)` → a fixed
  int64 (`database_utils.hpp:266`). The **row encoding is identical to v6** (`key_value` =
  pk u64 | payer u64 | varuint len | value). A table with 0 rows of a given index type is **skipped** in
  that section (its flattened id still advances), so `t_id`s are strictly increasing but may have gaps in
  the secondary sections; in `key_value` every table has its primary row(s), so the walked groups are
  contiguous.
- To read voters/accounts: parse `table_id_object` → `ordinal -> (code,scope,table)`, then walk
  `key_value_object`, joining each group's `t_id` to that map. Branch on the chain version (6 → commingled
  `contract_tables`; 8 → split sections).

## 4. ABI extraction

### 4.1 `account_object` row

Section `"eosio::chain::account_object"`. `FC_REFLECT(account_object,(name)(creation_date)(abi))`
(`account_object.hpp:107`); `id` not serialized:

```
name          : u64 LE  (account_name)
creation_date : u32 LE  (block_timestamp_type slot count since 2000-01-01, 500ms slots)
abi           : shared_blob  ->  [varuint len][len raw bytes]
```

The ABI lives in **`account_object.abi`**, NOT `account_metadata_object` (the metadata index holds
`abi_sequence`/`code_hash` only, no blob; `account_object.hpp:47-65`). Stable across v6/v7/v8.

### 4.2 `abi` blob encoding — the one corrected claim (Verdict V4-abi: **REFUTED as worded**)

There are **two** byte views, and they are not interchangeable:

- **The blob *payload*** (what `abi.data()/abi.size()` point to in memory) is a **bare fc-packed
  `abi_def`**, version string first, **with NO length prefix** (`set_abi` does `fc::raw::pack(ds, abi_def)`
  straight into the blob, `account_object.hpp:18-25`). **This** is what abieos consumes.
- **The serialized field inside the `account_object` row** prepends a `varuint32` length: the
  `shared_cow_string` serializer emits `varuint(size) || size bytes` (`database_utils.hpp:279-298`).

> **Therefore: when reading `abi` out of a snapshot row you MUST strip the leading `varuint32` length
> first, then hand the remaining `len` bytes (which begin with the `version` string) to
> `AbiHandle::from_bin`.** The original claim "loadable as-is with no extra unwrapping" is false *as
> worded* (it conflated the two views): the row field has the varuint; the inner payload has no varuint.

Empirical corroboration: the shipped `P:\eosrio\rs-abieos\abis\eosio.abi.bin` starts
`0e 65 6f 73 69 6f 3a 3a 61 62 69 2f 31 2e 32` = varuint `0x0e`(=14) then ASCII `eosio::abi/1.2` — i.e. the
*payload* begins directly with the version string and is fed straight to `set_abi_bin`.

Inner `abi_def` field order (`abi_def.hpp:179-180`), each `string` = `varuint len + bytes`, each `vector` =
`varuint count + elements`:

```
version (string, "eosio::abi/1.x" or "2.x")
types[]            : {new_type_name:string, type:string}
structs[]          : {name:string, base:string, fields[]{name:string,type:string}}
actions[]          : {name:u64, type:string, ricardian_contract:string}
tables[]           : {name:u64, index_type:string, key_names[]:string, key_types[]:string, type:string}
ricardian_clauses[]: {id:string, body:string}
error_messages[]   : {error_code:u64, error_msg:string}
abi_extensions[]   : {u16, vector<char>}
variants[]         : may_not_exist  (present only if bytes remain)
action_results[]   : may_not_exist  (present only if bytes remain)
```

### 4.3 Loading into rs_abieos

```rust
// payload = abi blob with the leading varuint32 length ALREADY STRIPPED
let handle = AbiHandle::from_bin(payload)?;   // handle.rs:33-37
```

Handle empty-ABI accounts: a length-0 blob (single `0x00` after the varuint strip → empty payload) must be
**skipped** (don't call `from_bin`; abieos rejects size 0 / empty version). `AbiHandle::from_hex` exists too
(`handle.rs:41-44`) if you carry ABIs as hex.

---

## 5. Decode path

For each `key_value` row collected from `contract_tables` (v6) / per-table KV section (v8) we have
`(code, scope, table, primary_key, payer, value, snapshot_block)`.

**Selection:**
- `table == name("voters")` AND `code == name("eosio")` → governance voters.
- `table == name("accounts")` (any `code`) → token balances; `scope` = holder account.

(Resolve `voters`/`accounts`/`eosio` to `u64` once at startup with `Abieos::string_to_name`,
`lib.rs:250-257`. Keep the hot loop keyed on `u64`.)

**Decode** (mirror delta-proto `decode_at`, `main.rs:336-365`):

```text
1. registry.active(code, snapshot_block) -> &mut AbiHandle   (per-worker cache; main.rs:227-241)
2. handle.decode_table_row_into(table, value, &mut out)      (handle.rs:88-101)
   - Ok           -> emit "data": <out JSON>
   - GetTypeForTable (table not in this ABI) -> retry at snapshot_block-1 if available
   - other error  -> raw-hex fallback
3. no ABI for (code, block) -> raw-hex fallback
```

**Raw-hex fallback** (every selected row still produces a doc): `"value":"<hex(value)>"`.

**`block_num` = snapshot head block.** Sources, preferred order:
1. Parse it out of the `block_state` section (the legacy_v3 / v8 block-header-state carries the head block
   number). *(Exact field offset within `snapshot_block_header_state_legacy_v3` is an open item — see §8.)*
2. **Fallback (reliable, zero-parse):** the EOSUSA inner filename is `snapshot-<block_id>.bin`, and the
   first 4 bytes of `<block_id>` (big-endian) are the block height — extract `block_num` from the filename
   `block_id` hex. This avoids decoding `block_state` entirely for phase 1.

**Hyperion doc shapes (NDJSON):**

- `voters` (decode `voter_info`): emit the decoded struct fields (`owner`, `proxy`, `producers[]`,
  `staked`, `last_vote_weight`, `proxied_vote_weight`, `is_proxy`, `flags1`, `reserved2`, `reserved3`),
  plus `block_num` and `present: true`.
- `accounts` (decode `account` struct → `{ balance: "<amount> <SYM>" }`): split `balance` into amount +
  symbol and emit:
  ```json
  {"code":"<code>","scope":"<holder>","symbol":"<SYM>","amount":<float>,"block_num":<n>,"present":true}
  ```

Resolve `code`/`scope`/`table`/`payer` `u64`→string only for the emitted doc (`Abieos::name_to_string`,
`lib.rs:273-288`), never in the decode hot loop.

---

## 6. Implementation blueprint — `crates/snapshot-load`

Auto-included by the workspace (`members = ["crates/*"]`, root `Cargo.toml:1-3`). Deps via
`{ workspace = true }`: `rs_abieos` (rust-backend), `anyhow`, `hex`, `clap`, `serde_json`, optionally
`hyperion-ship` (`crates/core`) for `read_varuint`.

```
crates/snapshot-load/
├── Cargo.toml
└── src/
    ├── main.rs     # CLI (clap), open .bin, dispatch on chain version, spawn scan + workers + writer
    ├── reader.rs   # header parse; section walker (find/skip by size); end-marker handling
    ├── abi.rs      # account_object section -> (code_u64 -> AbiHandle) registry (strip varuint, from_bin)
    ├── tables.rs   # contract_tables(v6) + per-table(v8) walkers -> emit KV rows to a channel
    ├── decode.rs   # decode_at + voters/accounts mapping (reuse delta-proto Registry/decode pattern)
    └── lib.rs      # pub mod reader, abi, tables, decode
```

### Pipeline (sequential scan + parallel decode, bounded memory)

- **Section scan is sequential** (length-prefixed; you cannot jump to `contract_tables` without walking
  prior section sizes). The `contract_tables`/per-table walk is also sequential (tree structure).
- **Decode parallelises.** Main thread walks rows and pushes `(code, scope, table, pk, payer, value_vec,
  block)` tuples onto a **bounded** `mpsc` channel; N worker threads each own a `Registry<AbiHandle>` (no
  locking — `AbiHandle`/`Abieos` are `Send`, not `Sync`) and decode; a single writer thread drains a second
  `mpsc` of NDJSON lines to a `BufWriter`.
- **Bounded memory:** cap in-flight rows (e.g. ~100k) so a 1.6 GB snapshot never fully materialises; the
  producer blocks (backpressure) when the queue is full.
- **ABIs first:** fully scan `account_object` into the shared `Arc<HashMap<u64, AbiHandle-or-hex>>` *before*
  walking contract rows (a row's ABI must already be loaded). The snapshot is a single point-in-time state,
  so there is exactly one ABI version per account — simpler than delta-proto's multi-version index.

### Reused hyperion-tools patterns (cite file:line)

- **Per-worker ABI registry / lazy parse:** `crates/delta-proto/src/main.rs:211-241` (`Registry::active`).
- **Decode + retry + raw-hex fallback:** `crates/delta-proto/src/main.rs:336-365` (`decode_at` + retry).
- **`mpsc` producer→workers→writer:** `crates/disk/src/disk.rs:330-341`.
- **Atomic stats (blocks/rows/decoded/raw):** `crates/delta-proto/src/main.rs:169-177`.
- **Reused decode `String` buffer:** `crates/delta-proto/src/main.rs:279` + `decode_table_row_into`.
- **`read_varuint(buf) -> Option<(value, bytes_consumed)>`:** `crates/core/src/delta.rs:10`.
- **Atomic checkpoint (temp-then-rename):** `crates/core/src/disk.rs:23-39` (optional, for resumability).
- **Work-stealing `AtomicU64` cursor** (`crates/disk/src/disk.rs:320-413`) — *not* applicable to the scan
  (sequential), but usable if we later shard decode by collected-row batches.

### rs_abieos signatures used

```rust
AbiHandle::from_bin(&[u8]) -> Result<AbiHandle, AbieosError>                 // handle.rs:33
AbiHandle::decode_table_row_into(&mut self, table:u64, bin:&[u8], out:&mut String) -> Result<(),_>  // handle.rs:88
AbiHandle::type_for_table(&self, table:u64) -> Option<&str>                  // handle.rs:47
Abieos::new() -> Abieos                                                       // lib.rs:209
Abieos::string_to_name(&self, &str) -> Result<u64,_>                          // lib.rs:250
Abieos::name_to_string(&self, u64) -> Result<String,_>                       // lib.rs:273
```

### Error handling & CLI

- `anyhow::Result` throughout; **per-row errors are logged + skipped** (raw-hex fallback), never abort the
  run. Hard errors only for: bad magic, file-version != 1, end-marker before a required section, chain
  version == 7 (unsupported) or unknown.
- CLI (clap):
  ```
  --snapshot <path>          # the .bin (or accept .tar.gz/.zst and decompress first — see §8)
  --out <path>               # NDJSON output (omit -> stdout / pure-throughput measure)
  --tables voters,accounts   # which tables to emit (default both)
  --threads <N>              # decode workers (default 8)
  --block-num <N>            # override head block (else derive from block_state / filename)
  --metadata-only            # drop value/data payload, keep keys (cold-tier, à la delta-proto)
  ```

---

## 7. Differential validation plan

> **RESULT (2026-06-01, Telos v6, block 470543854): PASSED.** spring-util v1.2.2 `snapshot info`
> confirmed version/head_block_num/block_id/chain_id/time exactly. spring-util `snapshot to-json`
> (full controller load → canonical JSON, v8 re-serialization) was diffed against our `--raw` output:
> **1,389,468 / 1,389,468** voters + token-`accounts` rows matched **byte-for-byte** on
> `(code, scope, table, primary_key, payer, value)` — 0 value mismatches, 0 payer mismatches, 0 rows
> only-in-oracle, 0 only-in-ours. Plus the in-tool invariants held: full-section consumption + `count ==
> Σ(all 6 index groups)` across all 2,464,131 tables. The v6 parse/extract is canonically correct.
> (Repro: `snapshots/tojson.sh`, `snapshots/diff.py`.)
>
> **RESULT (2026-06-01, Jungle4 v8, block 268849922): PASSED.** Same oracle diff on the v8 split-section
> layout: **75,849 / 75,849** voters + `accounts` rows byte-for-byte identical (0 value/payer mismatches,
> 0 set differences), with the v8 walk consuming the `key_value_object` section to its exact boundary and
> strictly-increasing `t_id`s throughout. Both v6 and v8 parse/extract are canonically correct; the reader
> also decompresses `.bin.zst` natively (ruzstd).

Validate decoded output against a trusted source **at the snapshot's head block**.

1. **Pin the block.** Get `block_num` from the snapshot (filename `block_id` → height; cross-check against
   `block_state`). Use a node/endpoint that still has that block in state, or `get_table_rows` "as of now"
   on a node restored *from this same snapshot*.

2. **`accounts` (token balances).** For a sample of `(code, scope)`:
   ```
   curl -s <api>/v1/chain/get_table_rows -d \
     '{"code":"eosio.token","scope":"<holder>","table":"accounts","json":true,"limit":100}'
   ```
   Compare each `balance` against our emitted `{symbol, amount}`. Expect exact string match on amount+symbol.

3. **`voters`.**
   ```
   curl -s <api>/v1/chain/get_table_rows -d \
     '{"code":"eosio","scope":"eosio","table":"voters","json":true,"limit":500}'
   ```
   Compare `voter_info` fields (`staked`, `last_vote_weight`, `producers`, `proxy`, …) row-by-row keyed by
   `owner`.

4. **Count reconciliation.** For `accounts`/`voters` (no secondary index) our emitted row count per
   `(code, scope, table)` equals `table_id_object.count`. In general `count` is the sum across all six
   index groups (see §3.2) — the reader already enforces `count == Σ(group counts)` for every table while
   walking (0 mismatches on the Telos v6 snapshot), a strong internal consistency check independent of any API.

5. **hyp-control / sync cross-check.** If a Hyperion instance has indexed the same chain, run a
   `hyp-control` table sync at the pinned block and diff our NDJSON against its `*-table-voters` /
   `*-table-accounts` documents (same `present`, `block_num`, key fields).

6. **Spring `spring-util` ground truth.** `spring-util snapshot to-json <bin>` (the JSON snapshot writer
   path) produces a canonical decode of the same file — diff selected sections to confirm our binary parse
   matches Spring's own reader exactly.

Pass criteria: 100% of decodable rows match the API; raw-hex-fallback rows are limited to tables a contract
never declared in its ABI (legal, ~0.1–0.2% on large chains, per delta-proto observations).

---

## 8. Risks & open questions

**Corrected / refuted (carry into code):**
- **V4-abi REFUTED as worded.** Strip the `varuint32` length from the `account_object.abi` field before
  `from_bin`; the inner payload (version-string first) has no length prefix. Conflating the two views is
  the #1 footgun. (`database_utils.hpp:279-298` vs `account_object.hpp:18-25`.)
- **Offset-4 u32 is the FILE-FORMAT version (`current_snapshot_version = 1`), NOT the chain version.**
  Multiple findings risked mislabeling it. The chain version (6/8) is the **first section's payload**. Do
  not branch parsing logic on offset-4; branch on `chain_snapshot_header.version`. (`snapshot.hpp:16`,
  `chain_snapshot.hpp:36-40`.)

**Version-gating risks:**
- v6 (commingled `contract_tables`) vs v8 (per-table sections) require **different walkers**; dispatch on
  the header version (`controller.cpp:2525-2530`). v7 is unsupported (Spring 1.0.1+ throws) — treat as a
  hard error. Telos will flip 6→8 at Savanna activation; the reader must already handle both.

**Uncertain (state the open question + exact source to resolve):**
- **`block_state` head-block field offset.** We have not byte-expanded
  `snapshot_block_header_state_legacy_v3` (v6) or `snapshot_block_state_data_v8`. To parse head `block_num`
  from `block_state`, read the reflection in `libraries/chain` referenced at `snapshot.cpp:~633-641` /
  `controller.cpp:2403-2424`. **Mitigation:** derive `block_num` from the filename `block_id` (first 4
  bytes BE), so phase 1 need not parse `block_state` at all.
- **Secondary-index skip correctness.** Sizes in §3.5 are derived from billable-size comments and the
  `std::array`/`float128→uint128` pack operators (`database_utils.hpp:338-368`, `raw.hpp:674-679`), not
  from a byte-level dump of each row in a real snapshot. Verify against the local Telos bin before relying
  on skip sizes (§9). A wrong skip size desynchronises the entire `contract_tables` walk.
- **`name`/`block_timestamp_type` widths** assumed `u64`/`u32` LE from the standard fc path; not
  re-verified line-by-line in `types.hpp`/`block_timestamp.hpp`. Very high confidence; verify empirically
  in §9.
- **Production writer parity.** Production snapshots may be written via the `random_access_file` writer
  rather than `ostream_snapshot_writer`; the read path (`snapshot.cpp:487-561`) implies identical framing
  and the local Telos bin parses correctly with this spec — but the producing writer was not read directly.

**Operational / memory risks:**
- **Packaging.** Inputs come as (a) bare `.bin`, (b) `.bin.zst` (EOS Nation), or (c) `.tar.gz` / `.tar.zst`
  with a single inner `snapshot-<block_id>.bin` (EOSUSA). The loader must sniff and decompress/untar to a
  bare `.bin` first. EOSUSA rate-limits (HTTP 429) — throttle CI downloads.
- **Big-snapshot memory.** 1.6 GB (Telos) today, **18 GB+ for EOS v8**. Never load the whole file or
  buffer all rows; use the bounded-channel pipeline (§6). The sequential scan is I/O-bound; decode scales
  with cores.
- **Decode mismatch.** A row decodes only against the **matching** ABI; system/secondary rows are not
  contract-ABI rows. Always keep the raw-hex fallback so every selected row yields a doc.

---

## 9. Empirical checks to run next on the local Telos `.bin`

Target file: `P:\eosrio\hyperion-tools\snapshots\telos-extract\snapshot-1c0bedeedf9a22b827d8f9b50f26c9310fdb6cb1e20177f99d979370334cbe70.bin`

Already confirmed (this session, via PowerShell on the real file): magic `0x30510550`, offset-4 `= 1`,
section 1 `chain_snapshot_header` (size 48, version **6**), section 2 `block_state` at offset 64 (size 3326).

Next:
1. **Walk all section headers to EOF.** From offset 8, loop `read u64 size; if size==UINT64_MAX stop; read
   u64 row_count; read name to NUL; record (offset, name, row_count, size); seek += 8+size`. Dump the full
   ordered section list and the **byte offset of `eosio::chain::account_object`** and **`contract_tables`**.
   Confirms §2.4 ordering and that v6 uses `contract_tables` (no per-table sections).
2. **Dump the first `account_object` row.** Read `u64 name`, `u32 creation_date`, `varuint abi_len`,
   `abi_len` bytes. Verify the ABI payload begins with `0x__ "eosio::abi/1."` (varuint len + version
   string) → confirms §4.2 (the row carries the varuint; payload is bare `abi_def`).
3. **Walk `contract_tables` to the first `eosio`/`voters` group.** Decode `table_id_object` rows
   (`code,scope,table,payer,count`) until `code==name("eosio") && table==name("voters")`; this also
   exercises the per-index `[count][rows]` skip and **validates the secondary-index skip sizes** (§3.5) by
   checking the walk stays aligned (each next `table_id_object` has plausible `name` values).
4. **Decode the first `eosio` voters KV row.** Load the `eosio` ABI from step 2 via
   `AbiHandle::from_bin(payload_after_varuint)`, then `decode_table_row_into(name("voters"), value, &mut s)`.
   Expect a JSON `voter_info` object. Spot-check `owner`/`staked` against
   `get_table_rows code=eosio scope=eosio table=voters` for that `owner` at the snapshot block.
5. **Cross-check counts.** Sum decoded `voters` rows vs the `eosio/eosio/voters` `table_id_object.count`.
