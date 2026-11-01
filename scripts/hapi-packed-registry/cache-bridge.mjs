// Linux CI only. Python validates the data graph independently; this bridge uses the actual pinned npm cache API.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFile,readdir,lstat,mkdir,writeFile,realpath} from 'node:fs/promises';
import {createRequire} from 'node:module';
import {dirname,join,relative,resolve,sep} from 'node:path';
import {fileURLToPath} from 'node:url';
const root=dirname(fileURLToPath(import.meta.url)),profile=JSON.parse(await readFile(join(root,'profile.json')));
assert.equal(process.platform,'linux');assert.equal(process.arch,'x64');assert.equal(process.env.GITHUB_ACTIONS,'true');
assert.equal(process.env.GITHUB_REPOSITORY,'eosrio/hyperion-tools');assert.equal(process.env.GITHUB_JOB,profile.job);
assert.ok(process.env.GITHUB_WORKFLOW_REF?.startsWith('eosrio/hyperion-tools/'+profile.workflow+'@'));
assert.equal(process.version,profile.runtime.node);assert.equal(process.env.NODE_OPTIONS,undefined);assert.equal(process.env.NODE_PATH,undefined);
const sha=b=>createHash('sha256').update(b).digest('hex'),pin=b=>({bytes:b.length,sha256:sha(b)}),max=256*1024**2;
assert.equal(sha(await readFile(process.execPath)),profile.runtime.nodeSha256);
const npmRoot=await realpath(process.env.REGISTRY94_NPM_ROOT),baseline=await readFile(join(root,'npm-members.json'));
assert.deepEqual(pin(baseline),{bytes:profile.runtime.npmMembers.bytes,sha256:profile.runtime.npmMembers.sha256});
const expected=JSON.parse(baseline).entries,actual=[];
async function visit(path){const s=await lstat(path),name='lib/node_modules/npm'+(path===npmRoot?'':'/'+relative(npmRoot,path).split(sep).join('/'));
 assert.ok(!s.isSymbolicLink());if(s.isDirectory()){actual.push({path:name,mode:s.mode&0o7777,type:'dir'});for(const n of await readdir(path))await visit(join(path,n));}
 else{assert.ok(s.isFile()&&s.nlink===1&&s.size<=max);actual.push({path:name,mode:s.mode&0o7777,type:'file',...pin(await readFile(path))});}assert.ok(actual.length<=2376);}
await visit(npmRoot);assert.equal(actual.length,2376);const sorted=v=>v.slice().sort((a,b)=>a.path<b.path?-1:a.path>b.path?1:0);
assert.deepEqual(sorted(actual),sorted(expected));assert.deepEqual(pin(await readFile(join(npmRoot,'package.json'))),profile.runtime.npmPackage);
// Loading executable npm dependencies only follows full original member/type/mode/content verification.
const require=createRequire(join(npmRoot,'package.json')),cacache=require('cacache'),pacote=require('pacote'),npa=require('npm-package-arg');
process.umask(0o022);
const [mode,specPath]=process.argv.slice(2);assert.ok(['export','outputs'].includes(mode));
const spec=JSON.parse(await readFile(specPath)),cache=resolve(spec.cacheRoot,'_cacache'),output=resolve(spec.output);
await mkdir(output,{recursive:false,mode:0o700});
const ordinaryFile=async(path,b)=>{await writeFile(path,b,{flag:'wx',mode:0o600});return pin(b);};
const under=(parent,path)=>{const rel=relative(parent,path);return rel!==''&&!rel.startsWith('..'+sep)&&rel!=='..'&&!rel.startsWith(sep);};
const serial=row=>({key:row.key,integrity:String(row.integrity),size:row.size,time:row.time,metadata:row.metadata??null});
const fetchBody=async(row)=>{assert.ok(Number.isSafeInteger(row.size)&&row.size>0&&row.size<=max);const result=await cacache.get(cache,row.key);
 assert.equal(result.data.length,row.size);assert.equal(String(result.integrity),String(row.integrity));return result.data;};
if(mode==='export'){
 const graph=JSON.parse(await readFile(spec.closure)),packages=Object.values(graph.packages).filter(p=>!profile.localPackages.includes(p.name)),names=new Set(packages.map(p=>p.name));
 const metadataNames=new Set(Object.keys(graph.requests).filter(name=>!profile.localPackages.includes(name))),urls=new Set(packages.map(p=>p.resolved));
 const options={cache,registry:profile.registry,preferOffline:true,updateNotifier:false};
 // These fetches are pinned to the already recorded resolver graph, not guessed direct roots.
 const warmed=[];
 for(const name of names){await pacote.packument(name,options);warmed.push({role:'selected-metadata',name});}
 for(const p of packages){await pacote.tarball.stream(p.resolved,async stream=>{let bytes=0;for await(const b of stream){bytes+=b.length;assert.ok(bytes<=max);}},{...options,integrity:p.integrity});
  warmed.push({role:'recorded-tarball',name:p.name,version:p.version,resolved:p.resolved,integrity:p.integrity});}
 const localPaths=new Set(spec.localPacks.map(p=>resolve(p))),all=await cacache.ls(cache),selected=[],excluded=[];
 for(const row of Object.values(all)){
  const key=row.key;let selectedKey=false;
  if(key.startsWith('make-fetch-happen:request-cache:')){
   const url=key.slice('make-fetch-happen:request-cache:'.length),u=new URL(url);assert.equal(u.origin,'https://registry.npmjs.org');assert.equal(u.search,'');assert.equal(u.hash,'');assert.equal(u.username,'');assert.equal(u.password,'');
   selectedKey=urls.has(url)||metadataNames.has(decodeURIComponent(u.pathname.slice(1)));assert.ok(selectedKey,'Unreferenced registry request key');
  }else if(key.startsWith('pacote:tarball:')){
   const from=key.slice('pacote:tarball:'.length);if(urls.has(from))selectedKey=true;
   else{const parsed=npa(from,spec.consumer);if(parsed.type==='file'){
    assert.ok(localPaths.has(resolve(parsed.fetchSpec)),'Foreign local package cache entry');excluded.push({key,reason:'fresh-rehearsal-local-pack-not-seed',integrity:String(row.integrity),size:row.size});continue;}
    assert.ok(parsed.registry&&names.has(parsed.name)&&graph.requests[parsed.name]?.includes(parsed.rawSpec),'Unexpected Git/local/unreferenced pacote key');selectedKey=true;}
  }else throw Error('Unexpected cache namespace: '+key);
  assert.ok(selectedKey);const body=await fetchBody(row);selected.push({row:serial(row),body});
 }
 const destination=join(output,'cache');await mkdir(destination,{mode:0o700});const target=join(destination,'_cacache');
 for(const{row,body}of selected)await cacache.put(target,row.key,body,{integrity:row.integrity,metadata:row.metadata??undefined,time:row.time});
 const final=await cacache.ls(target),entries=Object.values(final).map(row=>({...serial(row),originTime:all[row.key].time})).sort((a,b)=>a.key.localeCompare(b.key,'en'));
 assert.equal(entries.length,selected.length);await ordinaryFile(join(output,'catalog.json'),Buffer.from(JSON.stringify({entries,excluded,warmed},null,2)+'\n'));
 console.log(JSON.stringify({cache:destination,entries:entries.length,excludedLocal:excluded.length}));
}else{
 assert.equal(spec.role,'OUTPUT');const all=await cacache.ls(cache),found=[],directories=[],identities=new Map();
 for(const row of Object.values(all))if(row.key.startsWith('pacote:tarball:file:')){
  const parsed=npa(row.key.slice('pacote:tarball:'.length),spec.consumer),path=parsed.fetchSpec;
  if(parsed.type==='directory'){directories.push(serial(row));continue;}
  assert.equal(parsed.type,'file');assert.ok(under(resolve(spec.proofTmp),resolve(path)),'Local pack did not originate in actual proof tmp');
  const body=await fetchBody(row),identity=resolve(path)+'\0'+String(row.integrity),prior=identities.get(identity);
  if(prior){assert.deepEqual(pin(body),{bytes:prior.bytes,sha256:prior.sha256});prior.cacheEntries.push(serial(row));continue;}
  const name='actual-'+found.length+'.tgz';await ordinaryFile(join(output,name),body);
  const item={path:name,role:'OUTPUT',origin:'actual-proof-mutable-cache-after-original-terminal',cacheEntries:[serial(row)],...pin(body)};found.push(item);identities.set(identity,item);
 }
 assert.equal(found.length,2,'Missing actual original proof pack custody');await ordinaryFile(join(output,'packs.json'),Buffer.from(JSON.stringify(found,null,2)+'\n'));
 assert.ok(directories.every(row=>found.some(p=>p.cacheEntries.some(e=>e.integrity===row.integrity)&&p.bytes===row.size)),'Foreign unselected directory cache body');
 await ordinaryFile(join(output,'cache-catalog.OUTPUT.json'),Buffer.from(JSON.stringify({role:'OUTPUT',entries:Object.values(all).map(serial),unselectedDirectoryPacks:directories},null,2)+'\n'));
 console.log(JSON.stringify({role:'OUTPUT',packs:found.length}));
}
