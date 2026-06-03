// Tiny concurrent HTTP benchmark — host-native (no Docker NAT tax), keep-alive.
//   node bench.mjs <base> <concurrency> <durationSec> <path1> [path2 ...]
// Fires `concurrency` workers in a closed loop for `durationSec`, round-robining the given paths,
// and reports throughput + latency percentiles. Path list lets one run mix endpoint shapes.
import http from 'node:http';

const [base, cStr, dStr, ...rawPaths] = process.argv.slice(2);
const paths = rawPaths.map((p) => p.trim());
const concurrency = parseInt(cStr, 10);
const durationMs = parseFloat(dStr) * 1000;
const u = new URL(base);
const agent = new http.Agent({ keepAlive: true, maxSockets: concurrency });

const lat = [];
let done = 0, errors = 0, bytes = 0, started = 0, stop = false;

function hit(path) {
  return new Promise((resolve) => {
    const t0 = process.hrtime.bigint();
    const req = http.get({ host: u.hostname, port: u.port, path, agent }, (res) => {
      let n = 0;
      res.on('data', (c) => { n += c.length; });
      res.on('end', () => {
        const ms = Number(process.hrtime.bigint() - t0) / 1e6;
        lat.push(ms); bytes += n; done++;
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
    await hit(paths[i % paths.length]);
    i++;
  }
}

function pct(arr, p) {
  if (!arr.length) return 0;
  const s = [...arr].sort((a, b) => a - b);
  return s[Math.min(s.length - 1, Math.floor((p / 100) * s.length))];
}

const wall0 = process.hrtime.bigint();
started = Date.now();
const workers = Array.from({ length: concurrency }, (_, i) => worker(i));
setTimeout(() => { stop = true; }, durationMs);
await Promise.all(workers);
const wallSec = Number(process.hrtime.bigint() - wall0) / 1e9;

const rps = done / wallSec;
console.log(JSON.stringify({
  paths: paths.length === 1 ? paths[0] : `${paths.length} mixed`,
  concurrency, durationSec: +wallSec.toFixed(2),
  requests: done, errors,
  rps: Math.round(rps),
  mb_s: +(bytes / 1e6 / wallSec).toFixed(1),
  lat_ms: {
    avg: +(lat.reduce((a, b) => a + b, 0) / lat.length).toFixed(2),
    p50: +pct(lat, 50).toFixed(2),
    p90: +pct(lat, 90).toFixed(2),
    p99: +pct(lat, 99).toFixed(2),
    max: +pct(lat, 100).toFixed(2),
  },
}, null, 0));
