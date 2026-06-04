// Cross-check snapshot-load's decoded asset docs against a live AtomicAssets API (same atomicassets-js
// decode), proving decode parity. `immutable_data` is block-independent, so it must match exactly even
// if the snapshot trails the API's head; `mutable_data` may differ if it changed after the snapshot
// block (reported separately).
//
//   npm install            # once
//   node crosscheck.js [dbName] [N] [apiBase] [mongoUri]
//
// apiBase defaults to the WAX-TESTNET API; use https://wax.api.atomicassets.io for mainnet.
const { MongoClient } = require("mongodb");

const DB = process.argv[2] || "aatest_waxtest";
const N = parseInt(process.argv[3] || "100", 10);
const API = process.argv[4] || "https://test.wax.api.atomicassets.io";
const URI = process.argv[5] || "mongodb://localhost:27017";

// Canonical deep-equal: attribute key ORDER is irrelevant (eosio-contract-api stores JSONB).
const canon = (v) =>
  Array.isArray(v) ? v.map(canon)
  : v && typeof v === "object" ? Object.fromEntries(Object.keys(v).sort().map((k) => [k, canon(v[k])]))
  : v;
const eq = (a, b) => JSON.stringify(canon(a)) === JSON.stringify(canon(b));

(async () => {
  const c = new MongoClient(URI, { serverSelectionTimeoutMS: 5000 });
  await c.connect();
  const db = c.db(DB);
  const ours = await db.collection("atomicassets-assets")
    .aggregate([{ $match: { data: { $ne: {} } } }, { $sample: { size: N } }]).toArray();
  console.log(`sampled ${ours.length} assets (non-empty data) from ${DB}, diffing vs ${API}\n`);

  let immOk = 0, immBad = 0, mutOk = 0, mutDiff = 0, structOk = 0, structBad = 0, missing = 0;
  const failures = [];
  for (const a of ours) {
    let api;
    try {
      const j = await fetch(`${API}/atomicassets/v1/assets/${a.asset_id}`, { signal: AbortSignal.timeout(12000) }).then((r) => r.json());
      if (!j.success || !j.data) { missing++; continue; }
      api = j.data;
    } catch { missing++; continue; }

    if (api.collection?.collection_name === a.collection_name &&
        api.schema?.schema_name === a.schema_name &&
        String(api.template?.template_id ?? null) === String(a.template_id ?? null)) structOk++;
    else { structBad++; failures.push({ asset_id: a.asset_id, kind: "struct" }); }

    if (eq(api.immutable_data || {}, a.immutable_data || {})) immOk++;
    else { immBad++; failures.push({ asset_id: a.asset_id, kind: "immutable_data", ours: a.immutable_data, api: api.immutable_data }); }

    // mutable_data can legitimately differ if changed after the snapshot block — report, don't fail.
    if (eq(api.mutable_data || {}, a.mutable_data || {})) mutOk++; else mutDiff++;
  }

  console.log("RESULTS");
  console.log(`  structural (collection/schema/template): ${structOk} ok / ${structBad} mismatch`);
  console.log(`  immutable_data (exact):                  ${immOk} ok / ${immBad} mismatch`);
  console.log(`  mutable_data (exact):                    ${mutOk} ok / ${mutDiff} differ (may be post-snapshot edits)`);
  console.log(`  not found on API (burned/post-snapshot): ${missing}`);
  if (failures.length) {
    console.log(`\nFAILURES (up to 5):`);
    for (const f of failures.slice(0, 5)) console.log(JSON.stringify(f, null, 2));
  } else {
    console.log(`\nALL CHECKED ASSETS MATCH (immutable_data + structural) ✓`);
  }
  await c.close();
})().catch((e) => { console.error(e); process.exit(1); });
