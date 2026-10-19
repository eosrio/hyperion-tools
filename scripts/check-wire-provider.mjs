import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync} from 'node:fs';
import {resolve, dirname} from 'node:path';
import {fileURLToPath, pathToFileURL} from 'node:url';
import {execFileSync, spawnSync} from 'node:child_process';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const revision = '5e94b871691f4919f060dd0a6a603f2d40e2fa36';
const wire = resolve(process.argv[2] ?? '../hyperion-wire-plugin');
assert.equal(execFileSync('git', ['rev-parse', 'HEAD'], {cwd:wire, encoding:'utf8'}).trim(), revision,
  'Cross-language oracle must be the exact coordinated Wire source revision');
const api = await import(pathToFileURL(resolve(wire,'dist/index.js')).href);
const corpus = resolve(root,'crates/core/testdata/wire');
const read = path => JSON.parse(readFileSync(resolve(corpus,path),'utf8'));
for (const line of readFileSync(resolve(corpus,'SHA256SUMS'),'utf8').trim().split(/\r?\n/)) {
  const [,hash,path] = /^(\S+)  (.+)$/.exec(line);
  assert.equal(createHash('sha256').update(readFileSync(resolve(corpus,path))).digest('hex'),hash,path);
}
const empty = {version:'sysio::abi/1.2',types:[],structs:[],actions:[],tables:[],ricardian_clauses:[],error_messages:[],abi_extensions:[],variants:[],action_results:[],enums:[],protobuf_types:{file:[]}};
const groups = new Map();
function add(profileId, op, expected) {
  const group = groups.get(profileId) ?? [];
  group.push({op,expected}); groups.set(profileId,group);
}
let checks = 0;
for (const file of ['oracle-vectors.json','crypto-vectors.json','source-vectors.json','extra-vectors.json','boundary-vectors.json','ship-vectors.json']) {
  for (const vector of read(file).vectors) {
    if (vector.id === 'captured-ship-deltas') continue; // explicit source-pinned framing path below
    let profile;
    try {profile = api.resolveCodecProfile(vector.profileId);} catch {continue;}
    const abi = vector.abiPath ? read(vector.abiPath) : vector.abi ?? empty;
    let expected;
    try {
      expected = vector.operation === 'decodeAbi'
        ? api.decodeWireAbi(Buffer.from(vector.hex,'hex'),profile)
        : api.createWireValueCodec(abi,profile).decode(vector.typeName,Buffer.from(vector.hex,'hex'));
    } catch {
      add(profile.id,{operation:vector.operation,abi,typeName:vector.typeName,hex:vector.hex},undefined);
      continue;
    }
    const op = {operation:vector.operation,abi,typeName:vector.typeName,hex:vector.hex};
    add(profile.id,op,expected);
    if (vector.operation === 'decodeValue') {
      let encoded;
      try {encoded = Buffer.from(api.createWireValueCodec(abi,profile).encode(vector.typeName,expected)).toString('hex');}
      catch {
        add(profile.id,{operation:'encodeValue',abi,typeName:vector.typeName,value:expected},undefined);
        continue;
      }
      add(profile.id,{operation:'encodeValue',abi,typeName:vector.typeName,value:expected},
        encoded);
    }
  }
}
const shipAbi = read('ship.abi.json');
const capture = read('ship-capture.json');
const release = 'wire-sysio-7dede884-engineering-v1';
const current = 'wire-sysio-6508c9e8-engineering-v1';
const ship = api.createWireShipCodec(shipAbi,api.resolveCodecProfile(release));
for (const vector of capture.vectors.filter(v=>v.typeName === 'result')) {
  add(release,{operation:'decodeBundle',abi:shipAbi,hex:vector.hex},ship.decodeBundle(Buffer.from(vector.hex,'hex')));
}
const receipts=JSON.parse(readFileSync(resolve(root,'crates/core/testdata/wire-host-receipts.json'),'utf8'));
assert.equal(receipts.fixtureKind,'synthetic-serialization-only');
assert.equal(receipts.consensusValidation,false);
add(release,{operation:'decodeBundle',abi:receipts.handshake,hex:receipts.frameHex},receipts.expected);
for(const [typeName,hex,value] of [
  ['signed_block',receipts.blockHex,receipts.expected.block],
  ['transaction_trace[]',receipts.tracesHex,receipts.expected.traces],
]) {
  add(release,{operation:'decodeValue',abi:receipts.handshake,typeName,hex},value);
  add(release,{operation:'encodeValue',abi:receipts.handshake,typeName,value},hex);
}

// One canonical view with an exact same-block ordinal deployment transition.
const abi = {...empty,structs:[{name:'row',base:'',fields:[{name:'id',type:'uint64'},{name:'when',type:'block_timestamp_type'}]}],
  tables:[{name:'dynamic.table',index_type:'',key_names:[],key_types:[],type:'row',table_id:0,secondary_indexes:[{name:'by.full.key',key_type:'uint64',table_id:9}]}]};
const abi2 = structuredClone(abi); abi2.tables[0].name = 'renamed.table';
const chain = '11'.repeat(32); const block = '22'.repeat(32);
const deployments = [
  {id:'deployment:1',chainId:chain,contract:'7',from:{blockNum:1,ordinal:0},until:{blockNum:21,ordinal:4},abiHex:Buffer.from(api.encodeWireAbi(abi)).toString('hex'),profileId:current},
  {id:'deployment:2',chainId:chain,contract:'7',from:{blockNum:21,ordinal:4},abiHex:Buffer.from(api.encodeWireAbi(abi2)).toString('hex'),profileId:current},
];
const valueHex = Buffer.from(api.createWireValueCodec(abi).encode('row',{id:'18446744073709551615',when:'2025-01-01T00:00:00.500'})).toString('hex');
const source = {chainId:chain,blockId:block,blockNum:21,ordinal:3,abiDeployment:'deployment:1'};
const record = {kind:'contract_row_kv',present:true,code:'7',payer:'8',tableId:0,keyHex:'00ff00112233445566778899aabbccddeeff',valueHex};
const registry = api.createWireDeploymentRegistry(deployments);
for (const [rec,src] of [[record,source],[record,{...source,ordinal:4,abiDeployment:'deployment:2'}],
  [{...record,present:false,valueHex:''},source],
  [{kind:'contract_index_kv',present:true,code:'7',payer:'8',tableId:9,secondaryKeyHex:'00ff00',primaryKeyHex:record.keyHex},source],
  [{kind:'contract_index_kv',present:false,code:'7',payer:'9',tableId:9,secondaryKeyHex:'00ff00',primaryKeyHex:record.keyHex},source]]) {
  add(current,{operation:'decodeKvRecord',deployments,record:rec,source:src},registry.decodeKvRecord(rec,src));
}
const codec = api.createWireValueCodec(shipAbi);
const rawName = n => codec.decode('name', Buffer.from(BigInt(n).toString(16).padStart(16,'0').match(/../g).reverse().join(''),'hex'));
for (const present of [true,false]) for (const stream of ['contract_row_kv','contract_index_kv']) {
  const payload = stream === 'contract_row_kv'
    ? {code:rawName(7),payer:rawName(8),table_id:0,key:record.keyHex,value:present?valueHex:''}
    : {code:rawName(7),payer:rawName(8),table_id:9,sec_key:'00ff00',pri_key:record.keyHex};
  const bytes = codec.encode(stream,[`${stream}_v0`,payload]);
  add(current,{operation:'normalizeKvRow',abi:shipAbi,stream,present,hex:Buffer.from(bytes).toString('hex'),source},
    api.createWireShipCodec(shipAbi).normalizeKvRow(stream,present,bytes,source));
}
for (const profile of [current,release]) for (const slot of [0,1,0xffffffff]) {
  const expected = api.createWireValueCodec(empty,api.resolveCodecProfile(profile)).decode('block_timestamp_type',Buffer.from(Uint32Array.of(slot).buffer));
  add(profile,{operation:'slotToIso',slot},expected);
}

const binary = process.env.PROTOCOL_REPLAY_BIN ?? resolve(root,'target/debug',process.platform==='win32'?'protocol-replay.exe':'protocol-replay');
function run(profileId,ops) {
  const result = spawnSync(binary,['--protocol',profileId],{input:ops.map(op=>typeof op === 'string'?op:JSON.stringify(op)).join('\n')+'\n',encoding:'utf8',maxBuffer:64*1024*1024});
  assert.ifError(result.error);
  return {status:result.status,stderr:result.stderr,values:result.stdout.trim().split(/\r?\n/).filter(Boolean).map(line=>JSON.parse(line))};
}
for (const [profileId,group] of groups) {
  const actual = run(profileId,group.map(c=>c.op));
  assert.equal(actual.status,group.some(c=>c.expected === undefined)?1:0,actual.stderr);
  assert.equal(actual.values.length,group.length);
  for (let i=0;i<group.length;i++) {
    if (group[i].expected === undefined) assert.ok(actual.values[i].error,`Malformed fixture accepted: ${profileId} ${i}`);
    else assert.deepEqual(actual.values[i],{result:group[i].expected},`${profileId} operation ${i}: ${group[i].op.operation} ${group[i].op.typeName??''}`);
    checks++;
  }
}
// Failed decode/deployment selection must return a failed process, never success.
for (const op of [
  {operation:'decodeBundle',abi:shipAbi,hex:capture.vectors.find(v=>v.id==='captured-ship-block-result').hex.slice(0,-2)},
  {operation:'decodeKvRecord',deployments:[],record,source},
  {operation:'decodeKvRecord',deployments,record,source:{...source,ordinal:4}},
  {operation:'decodeKvRecord',deployments:[deployments[0],{...deployments[0],id:'overlap'}],record,source},
  {operation:'decodeKvRecord',deployments,record:{...record,tableId:65535},source},
]) {
  const failed = run(current,[op]); assert.notEqual(failed.status,0); assert.ok(failed.values[0].error); checks++;
}
const deploymentOp = {operation:'decodeKvRecord',deployments,record,source};
const floatingIntegers = JSON.stringify(deploymentOp).replaceAll('"blockNum":21','"blockNum":21.0')
  .replaceAll('"ordinal":3','"ordinal":3.0').replaceAll('"tableId":0','"tableId":0.0');
const numeric = run(current,[floatingIntegers]);
assert.equal(numeric.status,0,numeric.stderr);
assert.deepEqual(numeric.values[0],{result:registry.decodeKvRecord(record,source)});checks++;
for (const profile of ['antelope',release,'unknown-profile']) {
  const refused = run(profile,[deploymentOp]);assert.notEqual(refused.status,0);checks++;
}
console.log(`Wire provider integration: ${checks} complete Node/Rust output checks passed; source ${revision}`);
