/** Offline component check of unchanged captured bytes; not initialization or worker acceptance. */
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync, realpathSync, writeFileSync} from 'node:fs';
import {dirname, join, relative, resolve} from 'node:path';
import {pathToFileURL} from 'node:url';
import {Socket} from 'node:net';

const root = dirname(new URL(import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, '$1'));
const input = join(root, 'ship-preflight-20261020.json');
const output = resolve(process.argv[2]);
assert.equal(dirname(output), resolve(root));
const hash = value => createHash('sha256').update(value).digest('hex');
const inputBytes = readFileSync(input);
assert.equal(hash(inputBytes), 'b8edf652dd0ad265917bf0111ca48789ec7a3f1976f2113783202cc3dd38acb4');
const captured = JSON.parse(inputBytes);
assert.equal(captured.chainId, '9ccd94e823fda51363bd4265ad618a909c4985c445078c79f8d030fe9697b71d');
assert.equal(captured.profileId, 'wire-sysio-6508c9e8-engineering-v1');
assert.equal(captured.sourceRevision, '6508c9e8f4044df8d5acb082a7af3be6a80cd9a0');
const packageRoot = realpathSync('P:/eosrio/hyperion-tools/out/portable-state-workers/hyperion-history-api/node_modules/@eosrio/hyperion-wire-plugin');
const manifestBytes = readFileSync(join(packageRoot, 'dist/protocol-node-artifact.json'));
const manifest = JSON.parse(manifestBytes);
assert.equal(manifest.sourceRevision, '8b7d8a39f098bde77fa8d8f22a20a4fffc3e457b');
assert.equal(manifest.sourceModified, false);
let componentFiles = 0;
for (const file of manifest.files) {
    const path = realpathSync(join(packageRoot, file.path));
    const name = relative(packageRoot, path);
    assert.ok(name && !name.startsWith('..') && !name.includes(':'));
    assert.equal(hash(readFileSync(path)), file.sha256, file.path); componentFiles++;
}
const originalConnect = Socket.prototype.connect;
let networkAttempts = 0;
Socket.prototype.connect = function () {networkAttempts++; throw new Error('Network disabled for offline codec check');};
const observations = [];
try {
    const {createWireShipCodec, resolveCodecProfile} = await import(pathToFileURL(join(packageRoot, 'dist/index.js')).href);
    const schema = Buffer.from(captured.schema.base64, 'base64');
    assert.equal(hash(schema), captured.schema.sha256);
    const schemaValue = JSON.parse(schema);
    const requestFields = schemaValue.structs.find(value => value.name === 'get_blocks_request_v0').fields;
    const codec = createWireShipCodec(schemaValue, resolveCodecProfile(captured.profileId));
    const decodeBytes = record => {
        const bytes = Buffer.from(record.base64, 'base64');
        assert.equal(bytes.length, record.bytes); assert.equal(hash(bytes), record.sha256); return bytes;
    };
    for (const message of captured.messages) {
        const request = decodeBytes(message.request), response = decodeBytes(message.response);
        assert.deepEqual(Buffer.from(codec.encodeRequest(message.requestValue)), request);
        const expectedRequest = structuredClone(message.requestValue);
        // The documented Wire codec preserves ABI bool's uint8 carrier. Normalize only
        // these declared bool inputs; both representations must encode to exact source bytes.
        if (expectedRequest[0] === 'get_blocks_request_v0') {
            for (const flag of ['irreversible_only', 'fetch_block', 'fetch_traces', 'fetch_deltas']) {
                assert.equal(requestFields.find(field => field.name === flag).type, 'bool');
                assert.equal(typeof expectedRequest[1][flag], 'boolean');
                expectedRequest[1][flag] = Number(expectedRequest[1][flag]);
            }
        }
        assert.deepEqual(codec.decodeRequest(request), expectedRequest);
        assert.deepEqual(Buffer.from(codec.encodeRequest(codec.decodeRequest(request))), request);
        assert.deepEqual(codec.decodeResult(response), message.decoded);
        const bundle = codec.decodeBundle(response);
        assert.equal(bundle.rawFrameHex, response.toString('hex'));
        observations.push({kind: message.kind, requestSha256: hash(request), responseSha256: hash(response),
            deltaFraming: bundle.deltaFraming, streams: bundle.deltas?.map(([, value]) => ({name: value.name, rows: value.rows.length})),
            deltaRows: bundle.deltaRows?.length});
        assert.equal(hash(response), message.response.sha256);
    }
    const block = observations.find(value => value.kind === 'block1');
    assert.deepEqual(block.deltaFraming, {declaredCount: 8, actualCount: 9, compatibility: 'wire-resource-index-count-v1'});
    assert.equal(block.streams.length, 9);
    assert.ok(block.streams.some(value => value.name === 'resource_limits'));
    assert.ok(block.streams.some(value => value.name === 'resource_usage'));
    assert.equal(networkAttempts, 0);
} finally {Socket.prototype.connect = originalConnect;}
assert.equal(hash(readFileSync(input)), hash(inputBytes));
const report = {schemaVersion: 1, passed: true, runtime: {version: process.version, executable: process.execPath,
    executableSha256: hash(readFileSync(process.execPath))}, inputSha256: hash(inputBytes), packageRevision: manifest.sourceRevision,
    componentManifestSha256: hash(manifestBytes), componentFiles, observations, networkAttempts,
    requestBoolRepresentation: 'Documented uint8 carriers; original input and decoded request re-encode to exact captured bytes.',
    fullInitialTableCompletenessValidated: false, canonicalAbiBootstrapQualified: false, nativeWorkerAcceptance: false,
    limits: ['Offline installed Node codec against real original testnet bytes only; no initializer, canonical ABI frontier, worker, API, Rust or full testnet qualification.']};
writeFileSync(output, JSON.stringify(report, null, 2) + '\n', {flag: 'wx'});
console.log(JSON.stringify(report));
