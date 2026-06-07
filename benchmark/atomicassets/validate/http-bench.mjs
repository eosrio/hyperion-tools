#!/usr/bin/env node
// http-bench.mjs — end-to-end HTTP latency + throughput for the AtomicAssets read path, comparing one or
// more endpoints under the SAME query corpus. Cycle B made WormDB serve the identical eosio-contract-api
// shape + query params as the reference Postgres atomicassets-api, so the same URLs hit both.
//
//   WORMDB=http://127.0.0.1:6390 ATOMIC=https://wax.api.atomicassets.io \
//   N=50000 C=100 STATS_WORMDB=aa-wormdb OUT=wax-run node http-bench.mjs
//
// Targets:   WORMDB / ATOMIC = base URLs (set one or both).
// Load:      N = requests per target (ignored if DURATION set); DURATION = seconds of steady-state load
//            per target; C = concurrency; TIMEOUT_MS = per-request deadline (a timeout counts as error).
// Corpus:    SAMPLE = corpus size; SAMPLE_FROM = base URL to sample from (default WORMDB). Sampled across
//            newest+oldest pages for collection/owner variety. With 2+ targets the corpus is reduced to
//            the cross-target INTERSECTION (entities present on every target) so a divergent dataset (a
//            live API vs a lagging local) can't make targets do different work for the same URL; the
//            dropped counts land in the JSON `coverage` so any divergence is visible. Fair side-by-sides
//            still assume both targets are on the same chain/head.
// Mix:       MIX = override weights, e.g. MIX=point=50,coll=20,owner=10,faceted=10,browse=5,account=5
// Resource:  STATS_WORMDB / STATS_ATOMIC = container name to sample CPU/RSS via `docker stats` during
//            that target's run (skipped if docker is absent / the name is unset).
// Output:    OUT = results-file prefix (default "bench-results") -> writes <OUT>.json + <OUT>.md.
//
// Reports per-query-type + overall p50/p95/p99 (min/mean/max + a latency histogram in the JSON) and
// sustained req/s (successful only). Latency is the client-observed served-HTTP time — the apples-to-
// apples consumer number. Portable ESM (node or bun). Targets run sequentially so the client never
// self-contends. Non-2xx, timeouts, and network errors are counted as failures, never as fast responses.

import { spawn } from "node:child_process";
import { writeFileSync } from "node:fs";

const env = process.env;
const N = Number(env.N ?? 10000);
const C = Number(env.C ?? 50);
const DURATION = env.DURATION ? Number(env.DURATION) : 0; // seconds; >0 => duration-based
const TIMEOUT_MS = Number(env.TIMEOUT_MS ?? 10000); // per-request deadline; a timeout counts as an error
const SAMPLE = Number(env.SAMPLE ?? 400);
const OUT = env.OUT ?? "bench-results";
const P = "/atomicassets/v1";

const norm = (u) => u.replace(/\/+$/, "");
const now = () => performance.now();
const pick = (a) => a[(Math.random() * a.length) | 0];
const sum = (a) => a.reduce((x, y) => x + y, 0);
const f = (x) => (Number.isFinite(x) ? x.toFixed(2) : "—");

// fetch with a per-request deadline. A manual AbortController + clearTimeout (not AbortSignal.timeout) so
// the timer is freed the instant the request settles — no lingering 10s handles piling up over a big run.
async function fetchT(url) {
  const ac = new AbortController();
  const timer = setTimeout(() => ac.abort(), TIMEOUT_MS);
  try {
    return await fetch(url, { signal: ac.signal });
  } finally {
    clearTimeout(timer);
  }
}
async function getJson(url) {
  try {
    const r = await fetchT(url);
    return r.ok ? await r.json() : null;
  } catch {
    return null;
  }
}

// sample a real corpus (newest + oldest pages for collection/owner variety)
async function sampleCorpus(base, want) {
  const out = [];
  for (let page = 1; out.length < want && page <= 25; page++) {
    const j = await getJson(`${base}${P}/assets?limit=200&order=desc&page=${page}`);
    const d = j?.data ?? [];
    if (!d.length) break;
    out.push(...d);
  }
  const asc = await getJson(`${base}${P}/assets?limit=200&order=asc`);
  if (asc?.data) out.push(...asc.data);
  return out;
}

// Restrict a candidate list to entries that return data on EVERY target, so divergent datasets (a live
// API vs a lagging/pruned local WormDB) can't make the two targets do different work for the "same" URL.
// Bounded concurrency; a getJson failure/timeout counts as absent (conservatively dropped). This is the
// fairness guard for list queries (owner/collection/faceted), where a miss is an HTTP 200 with a smaller
// page rather than a flagged 404 — so without it, data-shape divergence would silently skew percentiles.
async function keepPresentOnAll(items, toUrl, bases) {
  const kept = [];
  let dropped = 0, idx = 0;
  async function worker() {
    while (idx < items.length) {
      const it = items[idx++];
      let present = true;
      for (const b of bases) {
        const j = await getJson(`${b}${toUrl(it)}`);
        const has = j && (Array.isArray(j.data) ? j.data.length > 0 : j.data != null);
        if (!has) { present = false; break; }
      }
      if (present) kept.push(it); else dropped++;
    }
  }
  await Promise.all(Array.from({ length: Math.min(12, items.length) }, worker));
  return { kept, dropped };
}

// ── stats helpers ──
const pctile = (a, p) => (a.length ? a[Math.round(p * (a.length - 1))] : NaN); // nearest-rank; 1.0 -> max, no collapse on small n
const HBINS = [1, 2, 5, 10, 20, 50, 100, 200, 500, 1000];
function stats(arr) {
  if (!arr.length) return { n: 0 };
  const a = [...arr].sort((x, y) => x - y);
  const histo = {};
  let lo = 0;
  for (const hi of HBINS) { histo[`<${hi}`] = a.filter((x) => x >= lo && x < hi).length; lo = hi; }
  histo[`>=${HBINS[HBINS.length - 1]}`] = a.filter((x) => x >= HBINS[HBINS.length - 1]).length;
  return { n: a.length, min: a[0], mean: sum(a) / a.length, p50: pctile(a, 0.5), p95: pctile(a, 0.95), p99: pctile(a, 0.99), max: a[a.length - 1], histo };
}

// ── docker-stats resource sampler (self-scheduling --no-stream polls) ──
function startStats(container) {
  if (!container) return null;
  const samples = [];
  let stopped = false;
  (function tick() {
    if (stopped) return;
    let out = "";
    let proc;
    try {
      proc = spawn("docker", ["stats", "--no-stream", "--format", "{{.CPUPerc}};{{.MemUsage}}", container], { stdio: ["ignore", "pipe", "ignore"] });
    } catch {
      stopped = true;
      return;
    }
    proc.stdout.on("data", (d) => (out += d.toString()));
    proc.on("error", () => (stopped = true)); // docker not installed
    proc.on("close", () => {
      // docker MemUsage unit is B / KiB / MiB / GiB / TiB (the "used" side, before the " / limit")
      const m = out.trim().match(/([\d.]+)%\s*;\s*([\d.]+)\s*([A-Za-z]+)/);
      if (m) {
        let mem = parseFloat(m[2]);
        const u = m[3].toLowerCase();
        if (u.startsWith("t")) mem *= 1024 * 1024;
        else if (u.startsWith("g")) mem *= 1024;
        else if (u.startsWith("m")) mem *= 1; // already MiB
        else if (u.startsWith("k")) mem /= 1024;
        else mem /= 1024 * 1024; // plain bytes -> MiB
        samples.push({ cpu: parseFloat(m[1]), mem });
      }
      if (!stopped) setTimeout(tick, 250);
    });
  })();
  return {
    stop() {
      stopped = true;
      if (!samples.length) return null;
      const cpu = samples.map((s) => s.cpu), mem = samples.map((s) => s.mem);
      return { container, samples: samples.length, cpuAvgPct: sum(cpu) / cpu.length, cpuPeakPct: Math.max(...cpu), memAvgMiB: sum(mem) / mem.length, memPeakMiB: Math.max(...mem) };
    },
  };
}

async function main() {
  const targets = [];
  if (env.WORMDB) targets.push({ name: "wormdb", base: norm(env.WORMDB), stats: env.STATS_WORMDB });
  if (env.ATOMIC) targets.push({ name: "atomic", base: norm(env.ATOMIC), stats: env.STATS_ATOMIC });
  if (!targets.length) {
    console.error("set WORMDB and/or ATOMIC to the target base URLs");
    process.exitCode = 1;
    return;
  }
  const sampleBase = norm(env.SAMPLE_FROM ?? env.WORMDB ?? env.ATOMIC);

  // ── 1) sample a real corpus ──
  console.log(`[bench] sampling ~${SAMPLE} assets from ${sampleBase} …`);
  const rows = await sampleCorpus(sampleBase, SAMPLE);
  if (!rows.length) {
    console.error(`[bench] no sample data from ${sampleBase} — is it serving /atomicassets/v1/assets?`);
    process.exitCode = 1;
    return;
  }
  const collOf = (a) => a.collection?.collection_name ?? a.collection_name ?? null;
  const schemaOf = (a) => a.schema?.schema_name ?? a.schema_name ?? null;
  let ids = [...new Set(rows.map((a) => a.asset_id).filter(Boolean))];
  let owners = [...new Set(rows.map((a) => a.owner).filter(Boolean))];
  let colls = [...new Set(rows.map(collOf).filter(Boolean))];
  let csPairs = [...new Map(rows.filter((a) => collOf(a) && schemaOf(a)).map((a) => [`${collOf(a)}|${schemaOf(a)}`, { c: collOf(a), s: schemaOf(a) }])).values()];
  console.log(`[bench] sampled: ${ids.length} ids, ${owners.length} owners, ${colls.length} collections, ${csPairs.length} (coll,schema) pairs`);

  // With 2+ targets, restrict the corpus to entities present on ALL of them so each target serves the
  // same rows even if the datasets diverge (live API vs lagging local). Dropped counts make divergence
  // visible; residual list-query page-size differences (a present owner with different counts across
  // out-of-sync targets) are not fully equalized — reflected in `coverage` for the reader to judge.
  let coverage = null;
  if (targets.length > 1) {
    const bases = targets.map((t) => t.base);
    console.log(`[bench] cross-target intersection over ${bases.length} targets…`);
    const ri = await keepPresentOnAll(ids, (id) => `${P}/assets/${id}`, bases);
    const ro = await keepPresentOnAll(owners, (o) => `${P}/assets?owner=${o}&limit=1`, bases);
    const rc = await keepPresentOnAll(colls, (c) => `${P}/assets?collection_name=${c}&limit=1`, bases);
    const rcs = await keepPresentOnAll(csPairs, (cs) => `${P}/assets?collection_name=${cs.c}&schema_name=${cs.s}&limit=1`, bases);
    coverage = { idsDropped: ri.dropped, ownersDropped: ro.dropped, collectionsDropped: rc.dropped, csPairsDropped: rcs.dropped };
    ids = ri.kept; owners = ro.kept; colls = rc.kept; csPairs = rcs.kept;
    const totalDropped = ri.dropped + ro.dropped + rc.dropped + rcs.dropped;
    if (totalDropped) console.log(`[bench] ⚠ dropped ${totalDropped} entities not present on all targets (ids -${ri.dropped}, owners -${ro.dropped}, colls -${rc.dropped}, pairs -${rcs.dropped}) — targets are NOT fully in sync; list-query page sizes may still differ`);
    if (!ids.length && !owners.length && !colls.length) {
      console.error("[bench] the targets' datasets do not overlap — nothing common to benchmark; are they on the same chain/head?");
      process.exitCode = 1;
      return;
    }
  }
  const corpus = { ids: ids.length, owners: owners.length, collections: colls.length, csPairs: csPairs.length, coverage };
  console.log(`[bench] corpus: ${ids.length} ids, ${owners.length} owners, ${colls.length} collections, ${csPairs.length} (coll,schema) pairs`);

  // ── 2) weighted query mix (default ~real-API-traffic-shaped; override with MIX=type=w,…) ──
  const W = { point: 35, coll: 25, owner: 15, faceted: 10, browse: 8, account: 7 };
  if (env.MIX) for (const part of env.MIX.split(",")) { const [k, v] = part.split("="); if (k in W && Number.isFinite(+v)) W[k] = +v; }
  const have = { point: ids.length, coll: colls.length, owner: owners.length, faceted: csPairs.length, browse: 1, account: owners.length };
  const MIX = [
    { type: "point", url: () => `${P}/assets/${pick(ids)}` },
    { type: "coll", url: () => `${P}/assets?collection_name=${pick(colls)}&limit=100` },
    { type: "owner", url: () => `${P}/assets?owner=${pick(owners)}&limit=100` },
    { type: "faceted", url: () => { const cs = pick(csPairs); return `${P}/assets?collection_name=${cs.c}&schema_name=${cs.s}&limit=100`; } },
    { type: "browse", url: () => `${P}/assets?limit=100` },
    { type: "account", url: () => `${P}/accounts/${pick(owners)}` },
  ].map((m) => ({ ...m, w: W[m.type] })).filter((m) => m.w > 0 && have[m.type] > 0);
  if (!MIX.length) {
    console.error("[bench] empty query mix — the sampled corpus has no usable dimensions, or every MIX weight is 0");
    process.exitCode = 1;
    return;
  }
  const totalW = sum(MIX.map((m) => m.w));
  const pickMix = () => {
    let r = Math.random() * totalW;
    for (const m of MIX) if ((r -= m.w) < 0) return m;
    return MIX[0];
  };

  async function run(target) {
    const lat = Object.fromEntries(MIX.map((m) => [m.type, []]));
    const all = [];
    let errs = 0, done = 0;
    // warm up (fill caches / JIT) before measuring + before sampling resources
    await Promise.all(Array.from({ length: Math.min(C, 20) }, async () => { for (let i = 0; i < 5; i++) { try { await fetchT(`${target.base}${pickMix().url()}`).then((r) => r.text()); } catch {} } }));
    const sampler = startStats(target.stats);
    const t0 = now();
    const deadline = DURATION ? t0 + DURATION * 1000 : 0;
    const per = DURATION ? Infinity : Math.ceil(N / C);
    await Promise.all(
      Array.from({ length: C }, async () => {
        for (let i = 0; i < per; i++) {
          if (DURATION && now() >= deadline) break;
          const m = pickMix();
          const s = now();
          // drain the body either way (frees the connection); count non-2xx + timeouts + network errors as failures
          const ok = await fetchT(`${target.base}${m.url()}`).then((r) => r.text().then(() => r.ok)).catch(() => false);
          const d = now() - s;
          done++;
          if (ok) { lat[m.type].push(d); all.push(d); } else errs++;
        }
      }),
    );
    const wall = now() - t0;
    const res = sampler ? sampler.stop() : null;
    const perType = Object.fromEntries(MIX.map((m) => [m.type, stats(lat[m.type])]));
    const overall = stats(all);
    const reqps = (all.length / wall) * 1000; // successful only — fast error pages must not inflate throughput

    console.log(`\n══ ${target.name}  (${target.base}) ══`);
    console.log(`${all.length} ok / ${done} sent in ${wall.toFixed(0)}ms → ${reqps.toFixed(0)} req/s (c=${C}${DURATION ? `, ${DURATION}s` : ""})  errors=${errs}${errs ? "  ⚠ results suspect — investigate errors" : ""}`);
    console.log(`  type      n      p50     p95     p99     max   (ms)`);
    for (const m of MIX) { const t = perType[m.type]; console.log(`  ${m.type.padEnd(8)} ${String(t.n).padStart(6)}  ${f(t.p50).padStart(6)}  ${f(t.p95).padStart(6)}  ${f(t.p99).padStart(6)}  ${f(t.max).padStart(6)}`); }
    console.log(`  ${"OVERALL".padEnd(8)} ${String(overall.n).padStart(6)}  ${f(overall.p50).padStart(6)}  ${f(overall.p95).padStart(6)}  ${f(overall.p99).padStart(6)}  ${f(overall.max).padStart(6)}`);
    if (res) console.log(`  resource (${res.container}): cpu avg ${f(res.cpuAvgPct)}% peak ${f(res.cpuPeakPct)}%  |  rss avg ${f(res.memAvgMiB)}MiB peak ${f(res.memPeakMiB)}MiB  (${res.samples} samples)`);
    return { name: target.name, base: target.base, reqps, sent: done, errors: errs, wallMs: wall, concurrency: C, overall, perType, resource: res };
  }

  const results = [];
  for (const t of targets) results.push(await run(t)); // sequential so the two targets don't contend on the client

  // ── side-by-side + results files ──
  if (results.length > 1) {
    console.log(`\n══ side-by-side ══`);
    for (const r of results) console.log(`  ${r.name.padEnd(8)} ${f(r.reqps).padStart(9)} req/s   p50=${f(r.overall.p50)}ms  p95=${f(r.overall.p95)}ms  p99=${f(r.overall.p99)}ms${r.errors ? `  (errors=${r.errors}!)` : ""}`);
  }

  const report = { generatedBy: "http-bench.mjs", params: { N, C, DURATION, SAMPLE, timeoutMs: TIMEOUT_MS, mix: W, sampleBase }, corpus, targets: results };
  writeFileSync(`${OUT}.json`, JSON.stringify(report, null, 2));

  const md = [];
  md.push(`# AtomicAssets HTTP benchmark`);
  md.push("");
  md.push(`Load: ${DURATION ? `${DURATION}s steady-state` : `${N} reqs`} per target, concurrency ${C}. Corpus: ${corpus.ids} ids / ${corpus.owners} owners / ${corpus.collections} collections / ${corpus.csPairs} (coll,schema) pairs sampled from \`${sampleBase}\`. Latency = client-observed served-HTTP time (ms); req/s = successful only.`);
  md.push("");
  md.push(`| target | req/s | p50 | p95 | p99 | max | errors | cpu avg/peak | rss avg/peak (MiB) |`);
  md.push(`|---|---:|---:|---:|---:|---:|---:|---:|---:|`);
  for (const r of results) {
    const o = r.overall, res = r.resource;
    md.push(`| ${r.name} | ${f(r.reqps)} | ${f(o.p50)} | ${f(o.p95)} | ${f(o.p99)} | ${f(o.max)} | ${r.errors} | ${res ? `${f(res.cpuAvgPct)}%/${f(res.cpuPeakPct)}%` : "—"} | ${res ? `${f(res.memAvgMiB)}/${f(res.memPeakMiB)}` : "—"} |`);
  }
  md.push("");
  for (const r of results) {
    md.push(`## ${r.name} — per query type`);
    md.push(`| type | n | p50 | p95 | p99 | max |`);
    md.push(`|---|---:|---:|---:|---:|---:|`);
    for (const m of MIX) { const t = r.perType[m.type]; md.push(`| ${m.type} | ${t.n} | ${f(t.p50)} | ${f(t.p95)} | ${f(t.p99)} | ${f(t.max)} |`); }
    md.push("");
  }
  writeFileSync(`${OUT}.md`, md.join("\n"));
  console.log(`\n[bench] wrote ${OUT}.json + ${OUT}.md`);
}

await main();
