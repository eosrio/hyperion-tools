import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {createRequire} from 'node:module';
const root='/root/qualification';
const state=JSON.parse(readFileSync(root+'/services24-c/worker-state.json','utf8'));
const require=createRequire(root+'/hyperion-history-api/package.json');
const {Client}=require('@elastic/elasticsearch');
const source=JSON.parse(readFileSync(root+'/hyperion-history-api/package-lock.json','utf8'));
const url=new URL(state.core.nodes[0].url);assert.equal(url.hostname,'127.0.0.1');assert.equal(state.core.run,'486e0a674a20');
const result={createdBy:'windows:codex:hyperion-tools',runtime:process.versions.bun?'Bun '+process.versions.bun:'Node '+process.version,
 clientVersion:source.packages['node_modules/@elastic/elasticsearch'].version,transportVersion:source.packages['node_modules/@elastic/transport'].version,
 undiciVersion:source.packages['node_modules/undici'].version,kind:'read-only info/close smoke on exclusively owned ES9.5.5 fixture',
 clientOrTransportOverrides:false,info:null,close:null};
const errorValue=error=>({name:String(error?.name).slice(0,100),code:error?.code??null,message:String(error?.message??error).slice(0,2000)});
process.on('uncaughtException',error=>{result.uncaughtException=errorValue(error);console.log(JSON.stringify(result));process.exit(2);});
let client;
try{client=new Client({node:url.href,requestTimeout:3000,maxRetries:0});const value=await client.info();
 assert.equal(value.version.number,'9.5.5');assert.equal(value.cluster_uuid,state.core.clusterUuid);result.info={status:'passed',serverVersion:value.version.number,clusterUuid:value.cluster_uuid};}
catch(error){result.info={status:'failed',error:errorValue(error)};}
if(client){try{let timer;try{await Promise.race([client.close(),new Promise((_,reject)=>{timer=setTimeout(()=>reject(new Error('client.close observation timeout')),3000);})]);}
 finally{clearTimeout(timer);}result.close={status:'passed'};}catch(error){result.close={status:'failed',error:errorValue(error)};}}
result.status=result.info?.status==='passed'&&result.close?.status==='passed'?'passed':'failed';console.log(JSON.stringify(result));
process.exitCode=result.status==='passed'?0:1;
