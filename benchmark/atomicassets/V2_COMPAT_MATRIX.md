# AtomicAssets v2 — compatibility matrix vs. our implementation

> **Sources.** Contract v2 PRs: `atomicassets/atomicassets-contract` PRs #1–#13 (all by `robrigo`,
> branched `v2 · Feature N`). Reference API new home (fork of PinkNetwork's `eosio-contract-api`,
> now community-maintained): `atomicassets/atomicassets-api`, v2 work on branch
> `test/v2-processor-coverage`. Our surface mapped from `crates/snapshot-load`, `crates/wseg-build`,
> `crates/atomicdata`. Researched 2026-06-05.
>
> **Update 2026-06-10.** The v2 ref-API branch is now `feat/atomicassets-v2`, released as
> **`2.0.0-rc2`** (jungle4-scoped); our fork at `p:\eosrio\atomicassets-api` has it locally, with our
> partition-parallel drain ported as upstream PR #72. The v2 **contract is deployed on jungle4** —
> the open questions below are now CLOSED against the deployed ABI (queried 2026-06-10):
>
> - **Feat 8 relocation DID NOT SHIP (was our only CRITICAL).** Deployed `collections_s` keeps
>   `serialized_data` **inline**; there is **no `collections_data`/`coldata` table** in the ABI, and
>   the merged ref-API v2 has no processor for one. `map_collection`'s inline read stays correct.
> - **Names pinned from the deployed ABI.** Tables: `assets, authorswaps, balances, collections,
>   config, holders, offers, schemas, schematypes, templates, templates2, tokenconfigs`. So:
>   **`templates2`** (not `templatedata`), `holders_s {asset_id, holder, owner}` confirmed, and there
>   is **no on-chain `moves` table** — moves are history (ref-API materializes
>   `atomicassets_moves[_assets]` from `logmove` actions; for us that's the ES/history tier, not state).
> - All v2 actions live on jungle4: `move/logmove, createtempl2, settempldata/logsetdatatl,
>   deltemplate, redtemplmax, setschematyp, createauswap/acceptauswap/rejectauswap`.
> - Ref-API v2 semantics (from the 2.0.0 migration in our fork): `holder` is **materialized at write
>   time** on `atomicassets_assets` (never derived as "= owner" at read time), with a one-time
>   `holder = owner` backfill — match that in our asset records.
> - Sidecar-tables-in-snapshot (old Q4): chain snapshots dump full contract state by construction, so
>   `holders/templates2/schematypes/authorswaps` will be present; verify empirically on the first v2
>   snapshot load.

## TL;DR

**v2 is overwhelmingly *additive*.** The existing on-chain row structs (`assets_s`, `templates_s`,
`collections_s`, `schemas_s`, `offers_s`, `config_s`) keep their field names, types, and **order** — so
every order-sensitive binary decoder we have (`aa_binfmt.rs`, the snapshot `map_*` fns, the
`atomicdata` blob format) **keeps working unchanged**. New capability ships as **sidecar tables**
(`templatedata`, `schematypes`, `authorswaps`, `holders`/`moves`, `collections_data`, `tokenconfigs`)
plus new actions. The `atomicdata` serialization format (the thing whose breakage would be
catastrophic) is **untouched** — `FORMAT {name, type}` and the varuint+type wire encoding are the same.

So there is **no decode-breaking change** — *with one thing to verify* (Feature 8 may relocate
collection `serialized_data` out of the main table). The real work is:

1. **Two of our load-bearing assumptions die.** (a) *Templates are immutable* — false now (mutable
   template data, max-supply reduction, deletion). (b) *`owner` is the custodian* — false now (the
   `move`/`holders` "dual ownership" model splits owner from holder).
2. **Six new sidecar tables** to ingest for reference-API parity.
3. **Five new live-overlay delta types** to keep the served head current.

Nothing forces an emergency change to keep *current* decoding correct; the work is **net-new parity
surface** for the v2 feature set.

## The matrix

Severity = impact on *our* code. **CRITICAL** = breaks an existing decode path; **HIGH** = breaks a
core assumption / needs a new ingest+index path; **MED** = parity gap, new sidecar to surface;
**LOW** = no action or trivial.

| PR | v2 feature | On-chain change | New actions | Our component(s) hit | Sev | What we must do |
|----|-----------|-----------------|-------------|----------------------|-----|-----------------|
| **#7** | **Feat 8 — CPU optimizations** | `collections_s.serialized_data` appears to be **cleared and relocated** to a new `collections_data` table (scope `coldata`); `tokenconfigs_s {standard, version}` added; config ABI typedefs reworked to getters | — (storage refactor) | `snapshot-load/atomicassets.rs:208` `map_collection` reads `serialized_data` **inline from `collections`**; wseg `TABLE_AA_COLL_FWD` | **CRITICAL (verify)** | If confirmed: read collection data from `collections_data`/`coldata`, not the main row. This is the **only** change that can break an existing decode path. **Verify against the merged diff before release.** |
| **#8** | **Feat 300 — "Renting" / `move`** | New `holders_s {asset_id, holder, owner}` table; ref-API adds a **`holder` column on assets** + `atomicassets_moves[_assets]` tables + `/v1/moves` | `move`, `logmove` (owner unchanged, holder updated) | wseg `TABLE_AA_BY_OWNER` + `aa_live` transfer semantics assume `owner == current custodian` | **HIGH** | Introduce **owner vs holder** as distinct dimensions. Ingest `holders`. Add a `holder` field to the asset fwd record + a `BY_HOLDER` inverted index. New overlay delta `MoveD {asset_id, new_holder}`. Decide query semantics: "owned by X" ≠ "held by X". |
| **#3** | **Feat 3+6 — Mutable templates & non-burnable⊕non-transferable** | New `template_data_s {template_id, schema_name, mutable_serialized_data}` table (`templatedata`, scope=collection); ref-API adds `mutable_data` on templates; XOR rule on `transferable`/`burnable` | `createtempl2`, `settempldata`, `logsetdatatl` (+ `createtempl` refactor) | wseg `TABLE_AA_TMPL_FWD` stores **immutable attrs only**; `aa_live` has **no template-mutation delta** | **HIGH** | Templates are no longer immutable. Ingest `templatedata`, decode `mutable_serialized_data` via the same `(collection, schema)` format, merge into the template fwd record. New overlay delta `TemplSetDataD`. (XOR rule is a validation invariant — no struct change.) |
| **#4** | **Feat 4 — Media types for schemas** | New `schema_types_s {schema_name, format_type[]}` where `FORMAT_TYPE {name, mediatype, info}` (`schematypes`, scope=collection); ref-API adds a `types` column on schemas | `setschematyp` | `atomicdata` format decode is **unaffected** (`FORMAT` unchanged); schema fwd store | **MED** | Decode does **not** break — media types are *sidecar metadata about* fields, not a new value encoding. For parity: ingest `schematypes`, attach per-field `{mediatype, info}` to the schema record/response. |
| **#5** | **Feat 5 — Changing collection authors** | New `author_swaps_s {collection_name, current_author, new_author, acceptance_date}` (`authorswaps`); on accept, `collections_s.author` mutates; ref-API adds `new_author_name` + date fields | `createauswap`, `acceptauswap`, `rejectauswap` (time-gated: 1wk delay active-perm, instant owner-perm, 3wk window) | `collections_s` decode fine; collection author was effectively static in our model | **MED** | Collection `author` can now change post-snapshot. Add overlay handling for `acceptauswap` → update collection fwd `author`. Ingest `authorswaps` for the pending-swap parity surface. |
| **#1** | **Feat 1 — Reduce max template supply** | No struct change; mutates `template.max_supply` in place | `redtemplmax` | Template fwd record `max_supply`; our model treats templates as immutable | **MED→LOW** | Snapshot decode fine. If we serve template `max_supply` from the live head, add an overlay path for `redtemplmax`. |
| **#2** | **Feat 2 — Delete unused templates** | No struct change; row erased when `issued_supply == 0`; ref-API tracks `deleted_at_block`/`deleted_at_time` (soft-delete) | `deltemplate` | `TABLE_AA_TMPL_FWD`, `TABLE_AA_BY_TMPL` assume templates persist | **MED→LOW** | Only *zero-issuance* templates (no assets reference them) are deletable → `BY_TMPL` posting lists are unaffected. Need a tombstone so template lookups 404 post-delete. Overlay delta `TemplDeleteD`. |
| **#6** | **Feat 7 — Deprecate backed assets** | `backed_tokens` field **stays** (backward-compatible); `backasset`/`mintasset` now reject backing | `backasset`→error, `mintasset` rejects `tokens_to_back`, `logbackasset` simplified | `snapshot-load` `map_asset` `backed_tokens` parse | **LOW** | `backed_tokens` decode unchanged; no new backing occurs. Existing backed balances persist (and may only shrink via unbacking). No action. |
| #9–#13 | Integration & test updates | Tests/CI only | — | — | — | None — track for the final merged ABI. |

## What this means component by component

### `crates/atomicdata` — **no change**
`FORMAT {name, type}`, the varuint-id (`id = index + 4`) sparse encoding, and the full type set
(`int*/uint*/fixed*/byte/bool/float/double/string/image/ipfs/bytes` + `[]`) are all untouched. Every
`serialized_data` / `*_serialized_data` blob in v2 decodes with the code we have. This is the single
most important "no-op" — it's the dependency whose breakage would have forced a rewrite.

### `crates/snapshot-load` (`atomicassets.rs`)
- **`map_collection` (line ~208): the one at-risk path.** If Feat 8 relocates collection
  `serialized_data` to `collections_data`/`coldata`, reading it inline yields empty data → silent
  parity loss on every collection's metadata. **Verify against the merged Feat 8 diff.**
- `map_asset`, `map_template`, `map_schema`, `map_offer`, `map_config`: field order preserved →
  **all still correct**. To reach v2 parity, *add* readers for the new sidecar tables: `holders`,
  `templatedata`, `schematypes`, `authorswaps` (+ `tokenconfigs`).
- `map_asset` should additionally surface the **`holder`** relationship (from `holders`), not just
  `owner`.

### `crates/wseg-build` (`aa_binfmt.rs`, `aa_builder.rs`, `aa_tables.rs`)
- **Asset fwd record** (`aa_binfmt.rs:~276`): add a `holder: u64` field. Bump the record `version`
  byte — the decoder already keys on it, so old/new segments coexist.
- **New inverted index `TABLE_AA_BY_HOLDER`** alongside `BY_OWNER` — "held by X" is a first-class
  query in v2.
- **Template fwd record** (`aa_binfmt.rs:~302`): extend to carry **mutable** attrs (today: immutable
  only) once `templatedata` is ingested; add a `deleted` tombstone for Feat 2.
- **Collection fwd record**: carry the pending author-swap state if we want swap parity.
- FNV1a-64 keying (`aa_tables.rs`) generalizes as-is — `by_holder` reuses `name::encode`, sidecar
  keys reuse `coll_schema_key`.

### `crates/wseg-build/aa_live.rs` — the live overlay grows new deltas
Current deltas: `MintD`, `TransferD`, `BurnD`, `SetDataD`. v2 needs:

| New delta | Triggered by | Effect on overlay |
|-----------|--------------|-------------------|
| `MoveD {asset_id, new_holder}` | `logmove` | update `holder` + `BY_HOLDER` index; **owner unchanged** |
| `TemplSetDataD {template_id, mutable}` | `settempldata`/`logsetdatatl` | update template fwd mutable attrs |
| `TemplDeleteD {template_id}` | `deltemplate` | tombstone template (issued_supply==0 guarantees no asset postings) |
| `TemplMaxD {template_id, new_max_supply}` | `redtemplmax` | update template `max_supply` |
| `AuthorSwapD {collection, new_author}` | `acceptauswap` | update collection fwd `author` |

Plus the existing dimension list (`DIM_OWNER…DIM_FACET`) gains **`DIM_HOLDER`**.

## Reference API parity surface (the new home)

`atomicassets/atomicassets-api` is the continuation of `eosio-contract-api` (TypeScript + Postgres 14,
SHiP "filler"). Its `test/v2-processor-coverage` branch is the canonical statement of what v2 parity
means downstream — use it as the spec for our served shapes:

- **`holder` column on `atomicassets_assets`**, `atomicassets_moves[_assets]` tables, **`/v1/moves`**
  endpoint + socket channel → our owner/holder split must match.
- **`mutable_data` on templates**, `deleted_at_block`/`deleted_at_time` soft-delete → our template
  fwd record extensions.
- **`types` column on schemas** (per-field media-type formatting) → our `schematypes` ingest.
- **`new_author_name` + date fields** for author swaps → our author-swap state.
- Handlers seen: `logmove`, `deltemplate`, `setschematyp`, `createauswap`/`acceptauswap`/
  `rejectauswap`, `redtemplmax`, `logsetdatatl`.

## Open questions to close before implementing

1. **Feat 8 collection storage (CRITICAL).** Does the *merged* contract actually move
   `serialized_data` out of `collections` into `collections_data`/`coldata`? The PR diff shows
   `serialized_data.clear()` + a new table; the ref-API v2 branch did *not* surface a `collections_data`
   processor in what we saw — so this may still be in flux. **Confirm against final ABI; it's our only
   decode-break risk.**
2. **Final table/struct names.** These are open PRs on feature branches — `templatedata` vs
   `tmplmutables`, `holders` vs the moves tables, scopes (`coldata`, per-collection) can change at
   merge. Pin names from the released ABI, not the PR text.
3. **Holder semantics for indexing.** When `move` sets holder≠owner, which does our default "account
   X's assets" query return? Match the ref-API: owner = title, holder = possession; likely need both
   queryable.
4. **Are templates/collections in the snapshot's sidecar tables?** Confirm the new multi-index tables
   appear in chain snapshots (so snapshot-load can seed them) vs. only reconstructable from history.

## Bottom line

No fire drill: our binary formats and `atomicdata` survive v2 intact, and current decoding stays
correct (modulo verifying Feat 8). The work is **net-new parity** — ingest six sidecar tables, add an
owner/holder split, and teach the live overlay five new deltas — to serve the full v2 feature set at
reference-API parity.
