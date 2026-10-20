// Independent Node consumer of JSON emitted by the real Rust disk worker and archive handlers.
// This checks ADR 0053 JSON compatibility, not Wire binary decode or testnet readiness.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const args = process.argv.slice(2);
assert.ok(args.length === 0 || (args.length === 1 && args[0] === '--release'), 'Usage: node scripts/check-sequence-json.mjs [--release]');
const child = spawnSync('cargo', [
  'test', '-p', 'action-proto', '-p', 'archive-server', '--locked', ...args,
  '--', 'sequence_public_json', '--nocapture',
], { cwd: fileURLToPath(new URL('..', import.meta.url)), encoding: 'utf8', timeout: 300_000, maxBuffer: 16 * 1024 * 1024 });
if (child.error || child.status !== 0) {
  process.stderr.write(child.stderr ?? '');
  process.stderr.write(child.stdout ?? '');
  throw child.error ?? new Error(`Rust public JSON tests exited ${child.status}`);
}

// These literals are independent of the Rust fixture and production integer formatter.
const expected = [
  '0', '1', '9007199254740991', '9007199254740992', '9007199254740993',
  '9223372036854775807', '9223372036854775808', '9223372036854775809', '18446744073709551615',
];
const records = [...child.stdout.matchAll(/HYPERION_SEQUENCE_JSON:(\{[^\r\n]*\})/g)].map(match => JSON.parse(match[1]));
const bySource = source => records.filter(record => record.source === source).map(record => record.body);
function receipt(value, sequence) {
  assert.equal(value.global_sequence, sequence);
  assert.equal(value.recv_sequence, sequence);
  assert.equal(value.auth_sequence.length, 1);
  assert.equal(value.auth_sequence[0].sequence, sequence);
}
assert.equal(records.length, 12, 'Missing or duplicate public-emitter evidence');
const [documents] = bySource('action-proto');
assert.equal(bySource('action-proto').length, 1);
assert.equal(documents.length, expected.length);
documents.forEach((document, index) => {
  assert.equal(document.global_sequence, expected[index]);
  assert.equal(document.block_num, 42);
  assert.equal(document.action_ordinal, index + 1);
  assert.equal(document.creator_action_ordinal, 0);
  assert.equal(document.code_sequence, 4294967295);
  assert.equal(document.abi_sequence, 4294967295);
  assert.equal(document.act.data, 'dead');
  receipt(document.receipts[0], expected[index]);
});
const single = bySource('archive-action');
assert.equal(single.length, expected.length);
single.forEach((action, index) => {
  assert.equal(action.global_sequence, expected[index]);
  assert.equal(action.block_num, 42);
  assert.deepEqual(action.data, { hex: 'DEAD' });
});
assert.equal(bySource('archive-block').length, 1);
const [block] = bySource('archive-block');
assert.equal(block.length, expected.length);
block.forEach((action, index) => {
  assert.equal(action.action_ordinal, index + 1);
  assert.equal(action.creator_action_ordinal, 0);
  assert.equal(action.receipt.code_sequence, 4294967295);
  assert.equal(action.receipt.abi_sequence, 4294967295);
  receipt(action.receipt, expected[index]);
});
assert.equal(bySource('archive-actions').length, 1);
const [{ actions }] = bySource('archive-actions');
assert.equal(actions.length, expected.length + 3);
actions.slice(0, 9).forEach((action, index) => {
  assert.equal(action.global_sequence, expected[8 - index]);
  assert.equal(action.block_num, 42);
  assert.equal(action.found, true);
});
assert.deepEqual(actions.slice(9).map(action => [action.global_sequence, action.found]), [
  ['18446744073709551615', false], ['9007199254740994', false], ['1', true],
]);
console.log(JSON.stringify({
  status: 'passed', consumer: process.version, publicJsonRecords: records.length,
  literalSequenceBoundaries: expected, sources: [...new Set(records.map(record => record.source))],
  wireBinaryDecodeQualified: false, wireTestnetQualified: false,
}));
