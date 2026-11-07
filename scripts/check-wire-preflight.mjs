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
  const chainId='9ccd94e823fda51363bd4265ad618a909c4985c445078c79f8d030fe9697b71d';
  const unsupported=/unsupported_feature:/;
  const deltas=['--from-disk',absent,'--abi-index',absent,'--start','1','--end','1','--out',sentinel];
  const cases=[
    ['abi-scanner',['--from-disk',absent,'--start','1','--end','1','--out',sentinel,'--checkpoint',checkpoint],/Wire history is sealed atomically/],
    // delta-proto and action-proto import Wire; an incomplete selection or a missing input is
    // refused before the output is opened.
    ['delta-proto',[...deltas,'--metadata-only','--blocks-dir',absent,'--chain-id',chainId],/unsupported_feature: --metadata-only/],
    ['delta-proto',[...deltas,'--chain-id',chainId],/--blocks-dir/],
    ['delta-proto',[...deltas,'--blocks-dir',absent],/--chain-id/],
    ['delta-proto',[...deltas,'--blocks-dir',absent,'--chain-id','ABC'],/--chain-id/],
    ['delta-proto',[...deltas,'--blocks-dir',absent,'--chain-id',chainId],/does-not-exist/],
    ['action-proto',['--from-disk',absent,'--abi-index',absent,'--start','1','--end','1','--out',sentinel],/does-not-exist/],
    ['archive-server',['--from-disk',absent,'--abi-index',absent],unsupported],
    ['snapshot-load',['--snapshot',absent,'--out',sentinel],unsupported],
    ['antelope-ship-archive',['check','--dir',absent],unsupported],
  ];
  for(const [name,args,reason] of cases){
    const path=resolve(bin,name+(process.platform==='win32'?'.exe':''));
    const result=spawnSync(path,['--protocol','wire-sysio-6508c9e8-engineering-v1',...args],{encoding:'utf8'});
    assert.ifError(result.error);assert.notEqual(result.status,0,name);
    assert.match(result.stderr,reason,`${name}: ${result.stderr}`);
    assert.equal(readFileSync(sentinel,'utf8'),'preserve existing output\n',name);
    assert.equal(existsSync(checkpoint),false,name);
  }
  console.log('Wire preflight: incompatible or incomplete paths and missing inputs refused before output/checkpoint mutation');
} finally {
  assert.equal(dirname(temp),resolve(tmpdir()));
  assert.ok(temp.includes('hyperion-wire-preflight-'));
  rmSync(temp,{recursive:true,force:true});
}
