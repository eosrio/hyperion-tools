// Inert VM control of only the source-selected v2 guard. Never import the bridge.
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
import test from 'node:test';
const source=readFileSync(new URL('./cache-bridge.mjs',import.meta.url),'utf8');
const selected=source.slice(source.indexOf("if(selection==='pidns-v2'){"),source.indexOf('assert.equal(sha('));
assert.ok(selected.startsWith("if(selection==='pidns-v2'){")&&selected.endsWith('}\n'));
async function guard(change=()=>{}){
 const data={uid:1001,gid:1001,status:{Uid:'1001 1001 1001 1001',Gid:'1001 1001 1001 1001',Groups:'',CapEff:'0',CapPrm:'0',CapInh:'0',CapAmb:'0',NoNewPrivs:'1'}};
 change(data);
 const context={kind:'current103-packed-registry-private-pidns-v2',profile:{modeled:true},uid:data.uid,gid:data.gid,initDumpable:1,
  init:{pidNamespace:'pid:[20]',mountNamespace:'mnt:[21]',init:{birth:100}},host:{pidNamespace:'pid:[10]'}};
 const fields=Array(20).fill('0');fields[19]='100';
 const links={'/proc/self/ns/pid':'pid:[20]','/proc/self/ns/mnt':'mnt:[21]','/proc/self':'2','/proc/1/ns/pid':'pid:[20]'};
 const contextPath='/model/context';
 const inputs={[contextPath]:JSON.stringify(context),'/model/profile-pidns.json':'modeled profile',
  '/proc/1/stat':'1 (modeled) '+fields.join(' '),'/proc/self/status':Object.entries(data.status).map(([key,value])=>key+':\t'+value).join('\n')+'\n'};
 const sandbox={assert,selection:'pidns-v2',root:'/model',join:(a,b)=>a+'/'+b,
  process:{pid:2,env:{REGISTRY_PIDNS_CONTEXT:contextPath},getuid:()=>data.uid,getgid:()=>data.gid},
  lstat:async()=>({isFile:()=>true,isSymbolicLink:()=>false,nlink:1,size:100,mode:0o600}),
  readFile:async path=>{assert.ok(Object.hasOwn(inputs,path));return inputs[path];},readlink:async path=>{assert.ok(Object.hasOwn(links,path));return links[path];}};
 await vm.runInNewContext('(async()=>{const pin=()=>({modeled:true});'+selected+'})()',sandbox);
}
test('source-selected bridge guard accepts fully unprivileged modeled context',async()=>{await guard();});
test('source-selected bridge guard refuses retained capability sets',async()=>{
 for(const key of ['CapEff','CapPrm','CapInh','CapAmb'])await assert.rejects(guard(data=>{data.status[key]='1';}));
});
test('source-selected bridge guard refuses every root credential and original root group',async()=>{
 for(const key of ['Uid','Gid'])for(let index=0;index<4;index++)await assert.rejects(guard(data=>{const ids=Array(4).fill('1001');ids[index]='0';data.status[key]=ids.join(' ');}));
 await assert.rejects(guard(data=>{data.gid=0;data.status.Gid='0 0 0 0';}));
 await assert.rejects(guard(data=>{data.uid=0;data.status.Uid='0 0 0 0';}));
});
test('source-selected bridge guard refuses supplementary root group and missing no_new_privs',async()=>{
 await assert.rejects(guard(data=>{data.status.Groups='0';}));await assert.rejects(guard(data=>{data.status.NoNewPrivs='0';}));
});
