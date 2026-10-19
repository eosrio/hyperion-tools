import assert from 'node:assert/strict';
import {readFileSync,writeFileSync} from 'node:fs';
import {createHash} from 'node:crypto';
import {resolve,dirname} from 'node:path';
import {fileURLToPath,pathToFileURL} from 'node:url';
import {execFileSync} from 'node:child_process';

const root=resolve(dirname(fileURLToPath(import.meta.url)),'..');
const wire=resolve(process.argv[2]);
const revision='5e94b871691f4919f060dd0a6a603f2d40e2fa36';
assert.equal(execFileSync('git',['rev-parse','HEAD'],{cwd:wire,encoding:'utf8'}).trim(),revision);
const api=await import(pathToFileURL(resolve(wire,'dist/index.js')).href);
const read=file=>JSON.parse(readFileSync(resolve(root,'crates/core/testdata/wire',file),'utf8'));
const handshake=read('ship.abi.json');
const profileId='wire-sysio-7dede884-engineering-v1';
const profile=api.resolveCodecProfile(profileId);
const codec=api.createWireValueCodec(handshake,profile);
const capture=read('ship-capture.json');
const original=capture.vectors.find(v=>v.id==='captured-ship-block-result');
const bundle=api.createWireShipCodec(handshake,profile).decodeBundle(Buffer.from(original.hex,'hex'));
const transfer=read('oracle-vectors.json').vectors.find(v=>v.id==='7dede884-token-transfer');
const action={account:'sysio.token',name:'transfer',authorization:[{actor:'alice',permission:'active'}],data:transfer.hex};
const transaction={expiration:'2026-10-19T21:09:00',ref_block_num:6349,ref_block_prefix:123,
  max_net_usage_words:0,max_cpu_usage_ms:0,delay_sec:0,context_free_actions:[],actions:[action,action],transaction_extensions:[]};
const packed=Buffer.from(codec.encode('transaction',transaction));
const packedTransaction={signatures:[],compression:0,packed_context_free_data:'',packed_trx:packed.toString('hex')};
const block=structuredClone(bundle.block);
block.transactions=[{cpu_usage_us:[23,456],trx:packedTransaction}];
const id=createHash('sha256').update(packed).digest('hex');
const traceAction=ordinal=>['action_trace_v1',{action_ordinal:ordinal,creator_action_ordinal:0,
  receipt:['action_receipt_v0',{receiver:'sysio.token',act_digest:'33'.repeat(32),global_sequence:String(ordinal),recv_sequence:String(ordinal),auth_sequence:[],code_sequence:0,abi_sequence:0}],
  receiver:'sysio.token',act:action,context_free:0,elapsed:'0',console:'',account_ram_deltas:[],except:null,error_code:null,return_value:''}];
const traces=[['transaction_trace_v0',{id,status:0,cpu_usage_us:479,net_usage_words:9,elapsed:'0',net_usage:'65',scheduled:0,
  action_traces:[traceAction(1),traceAction(2)],account_ram_delta:null,except:null,error_code:null,failed_dtrx_trace:null,partial:null}]];
const blockHex=Buffer.from(codec.encode('signed_block',block)).toString('hex');
const tracesHex=Buffer.from(codec.encode('transaction_trace[]',traces)).toString('hex');
const envelope=structuredClone(bundle.envelope);envelope[1].block=blockHex;envelope[1].traces=tracesHex;
const frameHex=Buffer.from(codec.encode('result',envelope)).toString('hex');
const fixture={schemaVersion:1,fixtureKind:'synthetic-serialization-only',consensusValidation:false,contractsDeployed:false,
  profileId,codecSourceRevision:revision,upstreamRevision:profile.upstreamRevision,
  sourceEvidence:{nativeReceipt:'libraries/chain/include/sysio/chain/block.hpp:8-30',traceCompatibility:'libraries/state_history/include/sysio/state_history/serialization.hpp:574-591',
    receiptStatusAndNetAbsent:true,cpuVectorMeaning:'billed CPU for each input action, in action order',
    traceCpuMeaning:'total_cpu_usage_us',traceNetWordsMeaning:'(trace.net_usage + 7) / 8'},
  caveat:'The captured block was transformed to exercise nonempty receipts and traces. Its block roots/signatures/IDs are not consensus-valid. This is not node-executed contract or testnet evidence.',
  handshake,packedTransactionHex:packed.toString('hex'),blockHex,tracesHex,frameHex,
  expected:api.createWireShipCodec(handshake,profile).decodeBundle(Buffer.from(frameHex,'hex'))};
const text=JSON.stringify(fixture,null,2)+'\n';
const path=resolve(root,'crates/core/testdata/wire-host-receipts.json');
if(process.argv.includes('--check'))assert.equal(readFileSync(path,'utf8'),text,'Receipt fixture needs deterministic regeneration');
else writeFileSync(path,text);
console.log(`Wire synthetic receipt fixture: ${createHash('sha256').update(text).digest('hex')}`);
