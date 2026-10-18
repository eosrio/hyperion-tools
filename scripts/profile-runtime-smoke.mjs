#!/usr/bin/env node
import cluster from 'node:cluster';
import { createRequire } from 'node:module';
import { getHeapStatistics } from 'node:v8';
import { join, resolve } from 'node:path';

const [mode, repo] = process.argv.slice(2);
const modes = ['cluster', 'uws', 'abieos', 'heap'];
if (!modes.includes(mode) || !repo) {
  console.error('Usage: <node-or-bun> scripts/profile-runtime-smoke.mjs <cluster|uws|abieos|heap> <history-api-checkout>');
  process.exit(2);
}
const require = createRequire(join(resolve(repo), 'package.json'));
const identity = {
  runtime: process.versions.bun ? 'bun' : 'node',
  version: process.versions.bun || process.versions.node,
  modules: process.versions.modules,
  platform: process.platform,
  arch: process.arch,
  mode,
};
function report(ok, details = {}) {
  console.log(JSON.stringify({ ...identity, ok, ...details }));
  if (!ok) process.exitCode = 1;
}
try {
  if (mode === 'cluster') {
    if (process.env.HYPERION_RUNTIME_PROBE_CHILD) {
      process.on('message', message => {
        if (message === 'ping') process.send?.({ pong: true, isWorker: cluster.isWorker, runtime: identity.runtime });
      });
    } else {
      const worker = cluster.fork({ HYPERION_RUNTIME_PROBE_CHILD: '1' });
      let settled = false;
      const timer = setTimeout(() => {
        if (settled) return;
        settled = true;
        worker.kill();
        report(false, { error: 'IPC timed out after 5s' });
      }, 5000);
      worker.once('error', error => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        worker.kill();
        report(false, { error: error.message });
      });
      worker.once('message', message => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        report(message.pong === true && message.isWorker === true && message.runtime === identity.runtime, { child: message });
        worker.kill();
      });
      worker.once('exit', () => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        report(false, { error: 'Worker exited before IPC response' });
      });
      worker.once('online', () => worker.send('ping'));
    }
  } else if (mode === 'uws') {
    const uws = require('uWebSockets.js');
    uws.App().get('/probe', response => response.end('ok')).listen('127.0.0.1', 0, async token => {
      if (!token) return report(false, { error: 'Loopback listen failed' });
      let ok = false;
      let details;
      try {
        const port = uws.us_socket_local_port(token);
        const response = await fetch('http://127.0.0.1:' + port + '/probe', { signal: AbortSignal.timeout(2000) });
        ok = response.status === 200 && await response.text() === 'ok';
        details = { exercised: 'native HTTP loopback request/listen/close' };
      } catch (error) {
        details = { error: error.message };
      } finally {
        try {
          uws.us_listen_socket_close(token);
        } catch (error) {
          ok = false;
          details = { ...details, cleanupError: error.message };
        }
      }
      report(ok, details);
    });
  } else if (mode === 'abieos') {
    const { Abieos } = require('@eosrio/node-abieos');
    const instance = Abieos.getInstance();
    let hex;
    let decoded;
    try {
      instance.loadAbi('probe', {
        version: 'eosio::abi/1.2', types: [],
        structs: [{ name: 'probe', base: '', fields: [{ name: 'value', type: 'uint32' }] }],
        actions: [], tables: [], ricardian_clauses: [], error_messages: [], abi_extensions: [], variants: [],
      });
      hex = instance.jsonToHex('probe', 'probe', { value: 42 });
      decoded = instance.hexToJson('probe', 'probe', hex);
    } finally {
      instance.cleanup();
    }
    report(hex.toLowerCase() === '2a000000' && decoded.value === 42,
      { exercised: 'native ABI load/encode/decode/cleanup', hex, decoded });
  } else {
    const heap = getHeapStatistics();
    report(typeof heap.used_heap_size === 'number' && heap.heap_size_limit > 0,
      { used: heap.used_heap_size, limit: heap.heap_size_limit, note: 'Runtime-specific heap semantics; not cross-VM capacity equivalence' });
  }
} catch (error) {
  report(false, { error: error.message });
}
