import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { basename, dirname, join } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { spawnSync } from 'node:child_process';

const probe = fileURLToPath(new URL('./profile-runtime-smoke.mjs', import.meta.url));
function runCleanupFailure(mode) {
  const base = realpathSync(tmpdir());
  const root = mkdtempSync(join(base, 'hyperion-runtime-probe-'));
  try {
    writeFileSync(join(root, 'package.json'), '{}');
    const moduleDir = join(root, 'node_modules', ...(mode === 'uws' ? ['uWebSockets.js'] : ['@eosrio', 'node-abieos']));
    mkdirSync(moduleDir, { recursive: true });
    writeFileSync(join(moduleDir, 'package.json'), JSON.stringify({ main: 'index.cjs' }));
    writeFileSync(join(moduleDir, 'index.cjs'), mode === 'uws'
      ? `module.exports = {
          App() { return { get() { return this; }, listen(_host, _port, callback) { callback({}); return this; } }; },
          us_socket_local_port() { return 1; },
          us_listen_socket_close() { throw new Error('injected close failure'); }
        };`
      : `module.exports = { Abieos: { getInstance() { return {
          loadAbi() {}, jsonToHex() { return '2a000000'; }, hexToJson() { return { value: 42 }; },
          cleanup() { throw new Error('injected cleanup failure'); }
        }; } } };`);
    const preload = join(root, 'preload.mjs');
    writeFileSync(preload, "globalThis.fetch = async () => ({ status: 200, text: async () => 'ok' });");
    return spawnSync(process.execPath, ['--import', pathToFileURL(preload).href, probe, mode, root], {
      encoding: 'utf8', timeout: 7000,
    });
  } finally {
    const target = realpathSync(root);
    if (dirname(target) !== base || !basename(target).startsWith('hyperion-runtime-probe-')) {
      throw new Error('Refusing to remove unexpected fixture directory');
    }
    rmSync(target, { recursive: true, force: true });
  }
}
for (const mode of ['uws', 'abieos']) {
  test(mode + ' cleanup failure emits one failed terminal result', () => {
    const child = runCleanupFailure(mode);
    assert.ifError(child.error);
    assert.equal(child.status, 1);
    const records = child.stdout.trim().split(/\r?\n/).filter(Boolean).map(line => JSON.parse(line));
    assert.equal(records.length, 1, JSON.stringify({ stdout: child.stdout, stderr: child.stderr }));
    assert.equal(records[0].ok, false);
    assert.match(JSON.stringify(records[0]), /injected (close|cleanup) failure/);
  });
}
