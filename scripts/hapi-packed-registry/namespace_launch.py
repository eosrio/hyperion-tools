"""Host-side CI namespace launcher. No host fallback and no local Windows execution."""
import argparse
import json
import os
from pathlib import Path
import platform
import shutil
import stat
import subprocess
import sys
import contracts as C
import namespace_contracts as N

HERE=Path(__file__).resolve().parent


def launch(product, output):
    C.need(platform.system() == 'Linux' and platform.machine() == 'x86_64' and os.geteuid() > 0, 'Original unprivileged Linux CI runner required')
    raw=(HERE/'profile-pidns.json').read_bytes();profile,_=C.load_contract(raw,(HERE/'npm-members.json').read_bytes())
    N.source_pins(profile,HERE);env=N.selected_environment(os.environ,profile);host=N.host_snapshot()
    C.need(host['uid'] > 0 and host['gid'] > 0, 'Original nonroot launcher UID/GID required before privileged namespace creation')
    product=product.resolve(strict=True);output=output.absolute();context=output.with_name(output.name+'.pidns')
    C.fresh_target(output.parent.resolve(strict=True),output);C.fresh_target(context.parent.resolve(strict=True),context)
    paths={name:Path(shutil.which(name)).resolve(strict=True) for name in ('sudo','unshare')}
    paths['python']=Path(sys.executable).resolve(strict=True)
    inputs={}
    for name,path in paths.items():
        info=path.lstat();C.need(stat.S_ISREG(info.st_mode) and not stat.S_IMODE(info.st_mode)&0o022, 'Ordinary privileged launcher input required')
        inputs[name]={'path':str(path),**C.pin(path.read_bytes())}
    request={'kind':N.KIND,'profile':C.pin(raw),'uid':host['uid'],'gid':host['gid'],'host':host,'environment':env,
             'product':str(product),'output':str(output),'contextRoot':str(context),'launcherInputs':inputs}
    argv=[str(paths['sudo']),'-n',str(paths['unshare']),'--mount','--pid','--fork','--mount-proc','--propagation','private',
          str(paths['python']),'-B',str(HERE/'namespace_init.py'),json.dumps(request,separators=(',',':'))]
    # No kill-child/timeout/retry. Original producer caps remain; CI teardown cannot supply closure.
    child=subprocess.run(argv,env=env,stdin=subprocess.DEVNULL)
    C.need(child.returncode == 0, 'Original isolated producer/init failed; no host fallback or retry')
    terminal_body=(context/'terminal.json').read_bytes();terminal=C.parse_json(terminal_body)
    receipt_body=(output/profile['artifact']['receipt']).read_bytes();receipt=C.parse_json(receipt_body)
    start_body=(context/'start.json').read_bytes();start=C.parse_json(start_body)
    C.need(C.pin(start_body) == receipt['executionContext']['contextPin'], 'Original producer init context differs')
    C.validate_receipt(receipt,profile,C.pin(raw));N.validate_completion(terminal,C.pin(raw),0,C.pin(receipt_body),start)
    proof={'kind':N.KIND,'profile':C.pin(raw),'originalLauncherExitCode':child.returncode,'producerReceipt':C.pin(receipt_body),
           'initStart':C.pin(start_body),'initTerminal':C.pin(terminal_body),
           'terminal':terminal,'launcherInputs':inputs,'teardownAbsenceUsedAsProof':False}
    with (output/profile['artifact']['context']).open('xb') as stream:
        stream.write(C.encoded(proof));stream.flush();os.fsync(stream.fileno())
    print(json.dumps({'isolatedProducer':'PASS','fullWireComponent':'NOT_RUN','contextProof':profile['artifact']['context']}),flush=True)


if __name__ == '__main__':
    parser=argparse.ArgumentParser();parser.add_argument('--product-root',type=Path,required=True);parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args();launch(args.product_root,args.output)
