// Inspect the atomicassets/atomicmarket collections snapshot-load wrote: per-collection counts +
// storage/index footprint + decoded-doc samples.
//
//   npm install            # once, to get the mongodb driver
//   node inspect.js [dbName] [mongoUri]
//
// dbName defaults to "<prefix>_<chain>" you loaded with (e.g. aatest_waxtest); mongoUri to localhost.
const { MongoClient } = require("mongodb");

const DB = process.argv[2] || "aatest_waxtest";
const URI = process.argv[3] || "mongodb://localhost:27017";

const fmtBytes = (n) => {
  if (n == null) return "-";
  const u = ["B", "KB", "MB", "GB", "TB"];
  let i = 0, v = n;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return `${v.toFixed(1)} ${u[i]}`;
};

(async () => {
  const c = new MongoClient(URI, { serverSelectionTimeoutMS: 5000 });
  await c.connect();
  const db = c.db(DB);
  const colls = (await db.listCollections().toArray())
    .map((x) => x.name)
    .filter((n) => n.startsWith("atomicassets-") || n.startsWith("atomicmarket-"))
    .sort();

  // On-disk footprint uses storageSize (WiredTiger-compressed, ~the actual bytes on disk), NOT size
  // (the uncompressed logical size). totalIndexSize is the on-disk index size. We report both.
  console.log(`\n=== db ${DB}: ${colls.length} AA/AM collections ===`);
  let onDisk = 0, idx = 0, logical = 0;
  console.log("collection".padEnd(30), "count".padStart(12), "disk".padStart(10), "indexes".padStart(10), "logical".padStart(10));
  for (const name of colls) {
    const st = await db.command({ collStats: name });
    onDisk += st.storageSize || 0;
    idx += st.totalIndexSize || 0;
    logical += st.size || 0;
    console.log(name.padEnd(30), String(st.count).padStart(12),
      fmtBytes(st.storageSize).padStart(10), fmtBytes(st.totalIndexSize).padStart(10), fmtBytes(st.size).padStart(10));
  }
  console.log("".padEnd(30, "-"));
  console.log("TOTAL".padEnd(30), "".padStart(12), fmtBytes(onDisk).padStart(10), fmtBytes(idx).padStart(10), fmtBytes(logical).padStart(10));
  console.log(`ON-DISK FOOTPRINT (data+indexes, compressed): ${fmtBytes(onDisk + idx)}   [logical data: ${fmtBytes(logical)}]`);

  const sample = async (coll, q, n, fields) => {
    if (!colls.includes(coll)) return;
    const docs = await db.collection(coll).find(q).limit(n).toArray();
    console.log(`\n--- ${coll} sample (${docs.length}) ---`);
    for (const d of docs) {
      const o = {};
      for (const f of fields) o[f] = d[f];
      if (d.data) o.data_keys = Object.keys(d.data);
      if (d.format) o.format_fields = d.format.map((x) => `${x.name}:${x.type}`);
      console.log(JSON.stringify(o));
    }
  };
  await sample("atomicassets-schemas", {}, 3, ["collection_name", "schema_name"]);
  await sample("atomicassets-templates", { data: { $ne: {} } }, 3, ["collection_name", "template_id", "schema_name", "issued_supply"]);
  await sample("atomicassets-assets", { data: { $ne: {} } }, 5, ["asset_id", "owner", "collection_name", "schema_name", "template_id"]);
  await sample("atomicassets-offers", {}, 2, ["offer_id", "sender", "recipient", "sender_asset_ids", "recipient_asset_ids"]);
  await sample("atomicmarket-sales", {}, 3, ["sale_id", "seller", "asset_ids", "listing_price", "listing_symbol", "state"]);
  await sample("atomicmarket-auctions", {}, 2, ["auction_id", "seller", "price", "token_symbol", "state"]);
  await c.close();
})().catch((e) => { console.error(e); process.exit(1); });
