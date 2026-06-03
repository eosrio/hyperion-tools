// Broad-load HTTP benchmark — sweeps requests across MANY distinct accounts so the server faults in
// a realistic working set (vs the single-hot-account battery, which barely touches the dataset).
// Use it to measure resident memory under production-like access, not just the best case.
//   node bench-broad.mjs <base> <concurrency> <durationSec> <accountFile> <pathTemplate>
//   pathTemplate uses {a} for the account, e.g. /api/accinfo/wax/{a}
import http from 'node:http';
import fs from 'node:fs';

const [base, cStr, dStr, acctFile, tmpl] = process.argv.slice(2);
const concurrency = parseInt(cStr, 10);
const durationMs = parseFloat(dStr) * 1000;
const accounts = fs.readFileSync(acctFile, 'utf8').split('\n').map((s) => s.trim()).filter(Boolean);
const u = new URL(base);
const agent = new http.Agent({ keepAlive: true, maxSockets: concurrency });

const lat = [];
let done = 0, errors = 0, bytes = 0, stop = false;

function hit(path) {
  return new Promise((resolve) => {
    const t0 = process.hrtime.bigint();
    const req = http.get({ host: u.hostname, port: u.port, path, agent }, (res) => {
      let n = 0;
      res.on('data', (c) => { n += c.length; });
      res.on('end', () => {
        lat.push(Number(process.hrtime.bigint() - t0) / 1e6); bytes += n; done++;
        if (res.statusCode >= 400) errors++;
        resolve();
      });
    });
    req.on('error', () => { errors++; done++; resolve(); });
  });
}

async function worker(id) {
  let i = id;
  while (!stop) {
    await hit(tmpl.replace('{a}', accounts[i % accounts.length]));
    i += concurrency; // stride so workers spread across the list at any instant
  }
}

function pct(arr, p) {
  if (!arr.length) return 0;
  const s = [...arr].sort((a, b) => a - b);
  return s[Math.min(s.length - 1, Math.floor((p / 100) * s.length))];
}

const wall0 = process.hrtime.bigint();
const workers = Array.from({ length: concurrency }, (_, i) => worker(i));
setTimeout(() => { stop = true; }, durationMs);
await Promise.all(workers);
const wallSec = Number(process.hrtime.bigint() - wall0) / 1e9;

console.log(JSON.stringify({
  tmpl, accounts: accounts.length, concurrency, durationSec: +wallSec.toFixed(2),
  requests: done, errors, rps: Math.round(done / wallSec), mb_s: +(bytes / 1e6 / wallSec).toFixed(1),
  lat_ms: {
    avg: +(lat.reduce((a, b) => a + b, 0) / lat.length).toFixed(2),
    p50: +pct(lat, 50).toFixed(2), p90: +pct(lat, 90).toFixed(2),
    p99: +pct(lat, 99).toFixed(2), max: +pct(lat, 100).toFixed(2),
  },
}, null, 0));
