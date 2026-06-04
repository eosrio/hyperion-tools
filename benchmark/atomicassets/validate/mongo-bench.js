// Benchmark the AtomicAssets faceted queries on an INDEXED Mongo state DB (the baseline the WormDB
// segment is compared against). Same 5 query shapes as aa-probe. Reports P50/P99 latency.
//   node mongo-bench.js [dbName] [iters]
const { MongoClient } = require("mongodb");

const DB = process.argv[2] || "aamain_wax";
const ITERS = parseInt(process.argv[3] || "200", 10);

const pctl = (a, p) => { a.sort((x, y) => x - y); return a[Math.min(a.length - 1, Math.floor(a.length * p))]; };
async function bench(iters, fn) {
  // warm
  await fn();
  const ds = [];
  for (let i = 0; i < iters; i++) { const t = process.hrtime.bigint(); await fn(); ds.push(Number(process.hrtime.bigint() - t) / 1000); } // µs
  return { p50: Math.round(pctl(ds, 0.5)), p99: Math.round(pctl(ds, 0.99)) };
}

(async () => {
  const c = new MongoClient("mongodb://localhost:27017", { serverSelectionTimeoutMS: 5000, maxPoolSize: 4 });
  await c.connect();
  const db = c.db(DB);
  const A = db.collection("atomicassets-assets");

  // pick representative keys: a sample asset that has a data.rarity, + a populated collection/owner.
  const sample = await A.findOne({ "data.rarity": { $exists: true } });
  if (!sample) { console.log("no asset with data.rarity in", DB); process.exit(1); }
  const { asset_id, owner, collection_name, schema_name } = sample;
  const rarity = sample.data.rarity;
  // a heavier collection if available: prefer the sample's collection
  const owner_n = await A.countDocuments({ owner }, { limit: 200000 });
  const coll_n = await A.countDocuments({ collection_name }, { limit: 500000 });
  console.log(`db ${DB} | keys: asset_id=${asset_id} owner=${owner}(${owner_n}+) collection=${collection_name}(${coll_n}+) rarity=${rarity}`);

  const Q1 = () => A.findOne({ asset_id });
  const Q2 = () => A.find({ owner }).sort({ asset_id: -1 }).limit(100).toArray();
  const Q3 = () => A.find({ collection_name, "data.rarity": rarity }).limit(100).toArray();
  const Q4 = () => A.find({ collection_name }).sort({ asset_id: -1 }).limit(100).toArray();
  const Q5 = () => A.find({}).sort({ asset_id: -1 }).skip(1000).limit(100).toArray();

  const rows = [
    ["Q1 point lookup by asset_id", await bench(ITERS, Q1)],
    ["Q2 owner page (sort id desc, 100)", await bench(ITERS, Q2)],
    ["Q3 collection + data.rarity (100)", await bench(ITERS, Q3)],
    ["Q4 collection page (100)", await bench(ITERS, Q4)],
    ["Q5 browse sort id desc skip 1000", await bench(ITERS, Q5)],
  ];
  console.log(`\n=== Mongo query latency (P50 / P99 over ${ITERS} iters, µs) ===`);
  for (const [n, r] of rows) console.log(`${n.padEnd(40)} ${String(r.p50).padStart(8)} µs / ${String(r.p99).padStart(8)} µs`);
  await c.close();
})().catch((e) => { console.error(e); process.exit(1); });
