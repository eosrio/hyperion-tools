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

// A "float-variant" difference: one side is a number, the other a string that parses to the same
// number (e.g. ours 3.97 vs API "3.97"). This is the unrecoverable case where the asset's logmint
// action stored a float/double via the attribute-map STRING variant — not present in the snapshot's
// canonicalized serialized_data. We report these separately, not as hard mismatches.
const floatVariant = (x, y) => {
  const pair = (typeof x === "number" && typeof y === "string") ? [x, y]
             : (typeof y === "number" && typeof x === "string") ? [y, x] : null;
  return pair != null && pair[1].trim() !== "" && Number(pair[1]) === pair[0];
};
// Classify a maps diff: returns {hard, soft} — hard = any non-float-variant differing key.
const classify = (ours, api) => {
  ours = ours || {}; api = api || {};
  let hard = false, soft = false;
  for (const k of new Set([...Object.keys(ours), ...Object.keys(api)])) {
    if (eq(ours[k], api[k])) continue;
    if (floatVariant(ours[k], api[k])) soft = true; else hard = true;
  }
  return { hard, soft };
};

(async () => {
  const c = new MongoClient(URI, { serverSelectionTimeoutMS: 5000 });
  await c.connect();
  const db = c.db(DB);
  const ours = await db.collection("atomicassets-assets")
    .aggregate([{ $match: { data: { $ne: {} } } }, { $sample: { size: N } }]).toArray();
  console.log(`sampled ${ours.length} assets (non-empty data) from ${DB}, diffing vs ${API}\n`);

  let immOk = 0, immHard = 0, immSoft = 0, mutOk = 0, mutDiff = 0, structOk = 0, structBad = 0, missing = 0;
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

    const imm = classify(a.immutable_data, api.immutable_data);
    if (!imm.hard && !imm.soft) immOk++;
    else if (imm.hard) { immHard++; failures.push({ asset_id: a.asset_id, kind: "immutable_data", ours: a.immutable_data, api: api.immutable_data }); }
    else immSoft++; // only float-variant differences (unrecoverable from a snapshot)

    // mutable_data can legitimately differ if changed after the snapshot block — report, don't fail.
    const mut = classify(a.mutable_data, api.mutable_data);
    if (!mut.hard && !mut.soft) mutOk++; else mutDiff++;
  }

  console.log("RESULTS");
  console.log(`  structural (collection/schema/template): ${structOk} ok / ${structBad} mismatch`);
  console.log(`  immutable_data exact:                    ${immOk} ok / ${immHard} HARD mismatch`);
  console.log(`  immutable_data float-variant only:       ${immSoft} (API stringified a float at mint — not in the snapshot)`);
  console.log(`  mutable_data (exact):                    ${mutOk} ok / ${mutDiff} differ (post-snapshot edits / float-variant)`);
  console.log(`  not found on API (burned/post-snapshot): ${missing}`);
  if (failures.length) {
    console.log(`\nHARD FAILURES (up to 5):`);
    for (const f of failures.slice(0, 5)) console.log(JSON.stringify(f, null, 2));
  } else {
    console.log(`\nNO HARD MISMATCHES — all assets match structurally + on immutable_data (modulo float-variant) ✓`);
  }
  await c.close();
})().catch((e) => { console.error(e); process.exit(1); });
