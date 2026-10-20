/** Offline exact-source Node codec check. This does not start a worker or establish initialization. */
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync, realpathSync} from 'node:fs';
import {dirname, join, relative, resolve} from 'node:path';
import {fileURLToPath, pathToFileURL} from 'node:url';
import {Socket} from 'node:net';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const evidence = join(root, 'docs/qualification/evidence/wire-testnet-20261020');
assert.ok(process.argv[2], 'Pass the exact built Wire package directory');
const wire = realpathSync(resolve(process.argv[2]));
const hash = value => createHash('sha256').update(value).digest('hex');
const inputPath = join(evidence, 'ship-preflight-20261020.json');
const inputBytes = readFileSync(inputPath);
assert.equal(hash(inputBytes), 'b8edf652dd0ad265917bf0111ca48789ec7a3f1976f2113783202cc3dd38acb4');
const captured = JSON.parse(inputBytes);
assert.equal(captured.chainId, '9ccd94e823fda51363bd4265ad618a909c4985c445078c79f8d030fe9697b71d');
assert.equal(captured.profileId, 'wire-sysio-6508c9e8-engineering-v1');
assert.equal(captured.sourceRevision, '6508c9e8f4044df8d5acb082a7af3be6a80cd9a0');
const reference = JSON.parse(readFileSync(join(evidence, 'protocol-node-artifact.json')));
const manifest = JSON.parse(readFileSync(join(wire, 'dist/protocol-node-artifact.json')));
assert.equal(manifest.sourceRevision, '8b7d8a39f098bde77fa8d8f22a20a4fffc3e457b');
assert.equal(manifest.sourceModified, false);
assert.deepEqual(manifest.files, reference.files, 'The built component must match all captured component file hashes');
assert.equal(manifest.files.length, 68);
for (const file of manifest.files) {
    const path = realpathSync(join(wire, file.path));
    const name = relative(wire, path);
    assert.ok(name && !name.startsWith('..') && !name.includes(':'));
    assert.equal(hash(readFileSync(path)), file.sha256, file.path);
}
const originalConnect = Socket.prototype.connect;
let networkAttempts = 0;
Socket.prototype.connect = function () {
    networkAttempts++;
    throw new Error('Network disabled for offline captured-input qualification');
};
const observations = [];
try {
    const {createWireShipCodec, resolveCodecProfile} = await import(pathToFileURL(join(wire, 'dist/index.js')).href);
    const schema = Buffer.from(captured.schema.base64, 'base64');
    assert.equal(schema.length, captured.schema.bytes);
    assert.equal(hash(schema), captured.schema.sha256);
    const schemaValue = JSON.parse(schema);
    const requestFields = schemaValue.structs.find(value => value.name === 'get_blocks_request_v0').fields;
    const codec = createWireShipCodec(schemaValue, resolveCodecProfile(captured.profileId));
    const decodeBytes = record => {
        const bytes = Buffer.from(record.base64, 'base64');
        assert.equal(bytes.length, record.bytes);
        assert.equal(hash(bytes), record.sha256);
        return bytes;
    };
    assert.deepEqual(captured.messages.map(message => message.kind), ['status', 'block1']);
    for (const message of captured.messages) {
        const request = decodeBytes(message.request);
        const response = decodeBytes(message.response);
        assert.deepEqual(Buffer.from(codec.encodeRequest(message.requestValue)), request);
        const expectedRequest = structuredClone(message.requestValue);
        // The Wire codec intentionally preserves ABI bool's uint8 carrier. Normalize only
        // these declared bool inputs; each representation must encode to the original bytes.
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
            deltaFraming: bundle.deltaFraming,
            streams: bundle.deltas?.map(([, value]) => ({name: value.name, rows: value.rows.length})),
            deltaRows: bundle.deltaRows?.length});
        assert.equal(hash(response), message.response.sha256);
    }
    const block = observations.find(value => value.kind === 'block1');
    assert.deepEqual(block.deltaFraming, {declaredCount: 8, actualCount: 9, compatibility: 'wire-resource-index-count-v1'});
    assert.deepEqual(block.streams, [
        {name: 'account', rows: 3}, {name: 'account_metadata', rows: 1}, {name: 'global_property', rows: 1},
        {name: 'protocol_state', rows: 1}, {name: 'permission', rows: 9}, {name: 'resource_limits', rows: 3},
        {name: 'resource_usage', rows: 3}, {name: 'resource_limits_state', rows: 1}, {name: 'resource_limits_config', rows: 1},
    ]);
    assert.equal(block.deltaRows, 23);
    assert.equal(networkAttempts, 0);
} finally {
    Socket.prototype.connect = originalConnect;
}
assert.equal(hash(readFileSync(inputPath)), hash(inputBytes));
console.log(JSON.stringify({passed: true, runtime: process.version, sourceRevision: captured.sourceRevision,
    inputSha256: hash(inputBytes), packageRevision: manifest.sourceRevision, componentFiles: manifest.files.length,
    observations, networkAttempts, fullInitialTableCompletenessValidated: false,
    canonicalAbiBootstrapQualified: false, nativeWorkerAcceptance: false}));
