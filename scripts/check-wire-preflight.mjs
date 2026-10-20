import assert from 'node:assert/strict';
import {mkdtempSync,writeFileSync,readFileSync,existsSync,rmSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {resolve,dirname} from 'node:path';
import {fileURLToPath} from 'node:url';
import {spawnSync} from 'node:child_process';

const root=resolve(dirname(fileURLToPath(import.meta.url)),'..');
const bin=resolve(root,process.env.WIRE_BIN_DIR ?? 'target/debug');
const temp=mkdtempSync(resolve(tmpdir(),'hyperion-wire-preflight-'));
try {
  const sentinel=resolve(temp,'existing.ndjson');writeFileSync(sentinel,'preserve existing output\n');
  const checkpoint=resolve(temp,'scan.checkpoint');
  const absent=resolve(temp,'does-not-exist');
  const cases=[
    ['abi-scanner',['--from-disk',absent,'--start','1','--end','1','--out',sentinel,'--checkpoint',checkpoint]],
    ['delta-proto',['--from-disk',absent,'--abi-index',absent,'--start','1','--end','1','--out',sentinel]],
    ['action-proto',['--from-disk',absent,'--abi-index',absent,'--start','1','--end','1','--out',sentinel]],
    ['archive-server',['--from-disk',absent,'--abi-index',absent]],
    ['snapshot-load',['--snapshot',absent,'--out',sentinel]],
    ['antelope-ship-archive',['check','--dir',absent]],
  ];
  for(const [name,args] of cases){
    const path=resolve(bin,name+(process.platform==='win32'?'.exe':''));
    const result=spawnSync(path,['--protocol','wire-sysio-6508c9e8-engineering-v1',...args],{encoding:'utf8'});
    assert.ifError(result.error);assert.notEqual(result.status,0,name);
    assert.match(result.stderr,/unsupported_feature:/,`${name}: ${result.stderr}`);
    assert.equal(readFileSync(sentinel,'utf8'),'preserve existing output\n',name);
    assert.equal(existsSync(checkpoint),false,name);
  }
  console.log('Wire preflight: six incompatible paths refused before output/checkpoint mutation');
} finally {
  assert.equal(dirname(temp),resolve(tmpdir()));
  assert.ok(temp.includes('hyperion-wire-preflight-'));
  rmSync(temp,{recursive:true,force:true});
}
