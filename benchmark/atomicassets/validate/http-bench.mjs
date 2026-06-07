#!/usr/bin/env node
// http-bench.mjs — end-to-end HTTP latency + throughput for the AtomicAssets read path, comparing one or
// more endpoints under the SAME query corpus. Cycle B made WormDB serve the identical eosio-contract-api
// shape + query params as the reference Postgres atomicassets-api, so the same URLs hit both.
//
//   WORMDB=http://127.0.0.1:6390 ATOMIC=https://wax.api.atomicassets.io N=10000 C=50 node http-bench.mjs
//
// Env: WORMDB / ATOMIC = base URLs of the targets (set one or both); N = requests per target; C =
// concurrency; SAMPLE = corpus size; SAMPLE_FROM = base URL to sample the corpus from (default WORMDB).
//
// It samples a REAL corpus (asset_ids / owners / collections / (coll,schema) pairs) from a source, then
// runs a weighted mixed workload (point / collection / owner / faceted / browse / account) against each
// target and reports per-query-type + overall p50/p95/p99 latency and sustained req/s. Latency is the
// client-observed served-HTTP time (what a consumer sees) — the apples-to-apples number across both.
// Resource use (CPU/RSS) is measured separately on each host (e.g. `docker stats`) while this runs.

const env = process.env;
const N = Number(env.N ?? 10000);
const C = Number(env.C ?? 50);
const SAMPLE = Number(env.SAMPLE ?? 400);
const P = "/atomicassets/v1";

const norm = (u) => u.replace(/\/+$/, "");
const targets = [];
if (env.WORMDB) targets.push({ name: "wormdb", base: norm(env.WORMDB) });
if (env.ATOMIC) targets.push({ name: "atomic", base: norm(env.ATOMIC) });
if (!targets.length) {
  console.error("set WORMDB and/or ATOMIC to the target base URLs");
  process.exit(1);
}
const sampleBase = norm(env.SAMPLE_FROM ?? env.WORMDB ?? env.ATOMIC);

const now = () => performance.now();
const pick = (a) => a[(Math.random() * a.length) | 0];
async function getJson(url) {
  try {
    const r = await fetch(url);
    return r.ok ? await r.json() : null;
  } catch {
    return null;
  }
}

// ── 1) sample a real corpus from one endpoint ──
console.log(`[bench] sampling ${SAMPLE} assets from ${sampleBase} …`);
const seed = await getJson(`${sampleBase}${P}/assets?limit=${SAMPLE}`);
const rows = seed?.data ?? [];
if (!rows.length) {
  console.error(`[bench] no sample data from ${sampleBase} — is it serving /atomicassets/v1/assets?`);
  process.exit(1);
}
const collOf = (a) => a.collection?.collection_name ?? a.collection_name ?? null;
const schemaOf = (a) => a.schema?.schema_name ?? a.schema_name ?? null;
const ids = [...new Set(rows.map((a) => a.asset_id).filter(Boolean))];
const owners = [...new Set(rows.map((a) => a.owner).filter(Boolean))];
const colls = [...new Set(rows.map(collOf).filter(Boolean))];
const csPairs = [...new Map(rows.filter((a) => collOf(a) && schemaOf(a)).map((a) => [`${collOf(a)}|${schemaOf(a)}`, { c: collOf(a), s: schemaOf(a) }])).values()];
console.log(`[bench] corpus: ${ids.length} ids, ${owners.length} owners, ${colls.length} collections, ${csPairs.length} (coll,schema) pairs`);

// ── 2) weighted query mix (roughly real-API-traffic-shaped; per-type latency is reported separately) ──
const MIX = [
  { type: "point", w: 35, url: () => `${P}/assets/${pick(ids)}` },
  { type: "coll", w: 25, url: () => `${P}/assets?collection_name=${pick(colls)}&limit=100` },
  { type: "owner", w: 15, url: () => `${P}/assets?owner=${pick(owners)}&limit=100` },
  { type: "faceted", w: 10, url: () => { const cs = pick(csPairs); return `${P}/assets?collection_name=${cs.c}&schema_name=${cs.s}&limit=100`; } },
  { type: "browse", w: 8, url: () => `${P}/assets?limit=100` },
  { type: "account", w: 7, url: () => `${P}/accounts/${pick(owners)}` },
].filter((m) => m.type === "point" || m.type === "browse" || (m.type === "faceted" ? csPairs.length : m.type === "account" || m.type === "owner" ? owners.length : colls.length));
const totalW = MIX.reduce((s, m) => s + m.w, 0);
function pickMix() {
  let r = Math.random() * totalW;
  for (const m of MIX) if ((r -= m.w) < 0) return m;
  return MIX[0];
}

const pctile = (arr, p) => (arr.length ? arr.sort((a, b) => a - b)[Math.min(arr.length - 1, Math.floor(arr.length * p))] : NaN);
const f = (x) => (Number.isFinite(x) ? x.toFixed(2) : "—");

async function run(target) {
  const lat = Object.fromEntries(MIX.map((m) => [m.type, []]));
  const all = [];
  let errs = 0;
  const per = Math.ceil(N / C);
  // warm up (fill caches / JIT) before measuring
  await Promise.all(Array.from({ length: Math.min(C, 20) }, async () => { for (let i = 0; i < 5; i++) { try { await fetch(`${target.base}${pickMix().url()}`).then((r) => r.text()); } catch {} } }));
  const t0 = now();
  await Promise.all(
    Array.from({ length: C }, async () => {
      for (let i = 0; i < per; i++) {
        const m = pickMix();
        const s = now();
        const ok = await fetch(`${target.base}${m.url()}`).then((r) => r.text()).then(() => true).catch(() => false);
        const d = now() - s;
        if (ok) { lat[m.type].push(d); all.push(d); } else errs++;
      }
    }),
  );
  const wall = now() - t0;
  const done = per * C;
  console.log(`\n══ ${target.name}  (${target.base}) ══`);
  console.log(`${done} reqs in ${wall.toFixed(0)}ms → ${((done / wall) * 1000).toFixed(0)} req/s (c=${C})  errors=${errs}`);
  console.log(`  type      n      p50     p95     p99   (ms)`);
  for (const m of MIX) {
    const a = lat[m.type];
    console.log(`  ${m.type.padEnd(8)} ${String(a.length).padStart(6)}  ${f(pctile(a, 0.5)).padStart(6)}  ${f(pctile(a, 0.95)).padStart(6)}  ${f(pctile(a, 0.99)).padStart(6)}`);
  }
  console.log(`  ${"OVERALL".padEnd(8)} ${String(all.length).padStart(6)}  ${f(pctile(all, 0.5)).padStart(6)}  ${f(pctile(all, 0.95)).padStart(6)}  ${f(pctile(all, 0.99)).padStart(6)}`);
  return { name: target.name, reqps: (done / wall) * 1000, p50: pctile(all, 0.5), p99: pctile(all, 0.99) };
}

const results = [];
for (const t of targets) results.push(await run(t)); // sequential so the two targets don't contend on the client

if (results.length > 1) {
  console.log(`\n══ side-by-side ══`);
  for (const r of results) console.log(`  ${r.name.padEnd(8)} ${f(r.reqps).padStart(9)} req/s   p50=${f(r.p50)}ms  p99=${f(r.p99)}ms`);
}
