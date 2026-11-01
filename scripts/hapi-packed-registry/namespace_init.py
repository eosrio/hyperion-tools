"""Source-pinned Linux PID1 reaper. Never import or execute this entry point on Windows."""
import ctypes
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import time
import contracts as C
import namespace_contracts as N

HERE = Path(__file__).resolve().parent


def write(path, value):
    body=C.encoded(value)
    with path.open('xb') as stream:
        os.fchmod(stream.fileno(),0o600)
        stream.write(body);stream.flush();os.fsync(stream.fileno())
    return C.pin(body)


def enter(request):
    C.need(platform.system() == 'Linux' and platform.machine() == 'x86_64' and os.getpid() == 1
           and os.getppid() == 0 and os.geteuid() == 0, 'Actual new Linux namespace init required')
    profile_bytes=(HERE/'profile-pidns.json').read_bytes()
    profile,_=C.load_contract(profile_bytes,(HERE/'npm-members.json').read_bytes())
    N.source_pins(profile,HERE)
    C.need(request['kind'] == N.KIND and request['profile'] == C.pin(profile_bytes)
           and request['uid'] == request['host']['uid'] > 0 and request['gid'] == request['host']['gid'] >= 0, 'Original launcher identity required')
    selected=N.selected_environment(request['environment'],profile)
    before=N.snapshot()
    C.need(before['pidNamespace'] != request['host']['pidNamespace'] and before['mountNamespace'] != request['host']['mountNamespace']
           and before['procMounted'] and before['procSelf'] == '1' and before['initNamespace'] == before['pidNamespace'], 'Fresh procfs and isolated init before effects required')
    libc=ctypes.CDLL(None,use_errno=True)
    libc.prctl.argtypes=[ctypes.c_int,ctypes.c_ulong,ctypes.c_ulong,ctypes.c_ulong,ctypes.c_ulong];libc.prctl.restype=ctypes.c_int
    C.need(libc.prctl(38,1,0,0,0) == 0, 'Cannot set inherited no_new_privs')
    os.setgroups([]);os.setgid(request['gid']);os.setuid(request['uid'])
    initial=N.snapshot();N.validate_snapshot(initial,request['host'],request['uid'],request['gid'],init=True)
    # setuid/setgid reset dumpability. Restore only the already-unprivileged PID1's
    # normal same-user proc visibility; never retain capabilities or bypass proc guards.
    C.need(libc.prctl(4,1,0,0,0) == 0 and libc.prctl(3,0,0,0,0) == 1, 'Unprivileged init proc visibility required')
    rows=N.census();C.need(set(rows) == {1}, 'Private init starts alone; no host/context fallback')
    context=Path(request['contextRoot']);output=Path(request['output']);repo=Path(request['product'])
    C.fresh_target(context.parent,context);C.fresh_target(output.parent,output)
    C.need(context == output.with_name(output.name+'.pidns') and context.parent == output.parent, 'Exact owned context sibling required')
    context.mkdir(mode=0o700)
    with (context/'profile.json').open('xb') as stream:
        stream.write(profile_bytes);stream.flush();os.fsync(stream.fileno())
    os.chmod(context/'profile.json',0o600)
    start={'kind':N.KIND,'profile':C.pin(profile_bytes),'uid':request['uid'],'gid':request['gid'],'host':request['host'],
           'init':initial,'initDumpable':1,'startedMonotonic':time.monotonic(),'launcherInputs':request['launcherInputs']}
    write(context/'start.json',start)
    env={**selected,'REGISTRY_PIDNS_CONTEXT':str(context/'start.json'),'REGISTRY_PROFILE':'pidns-v2'}
    argv=[sys.executable,'-B','-u',str(HERE/'producer.py'),'--profile','pidns-v2','--product-root',str(repo),'--output',str(output)]
    child=subprocess.Popen(argv,env=env,stdin=subprocess.DEVNULL)  # Original inherited CI streams; no proxy transform.
    child_pid=child.pid;reaped=[];reap_count=0;producer_exit=None;scan_errors=[];closure=[];leftovers=[]
    while True:
        while True:
            try:pid,status=os.waitpid(-1,os.WNOHANG)
            except ChildProcessError:break
            if pid == 0:break
            code=os.waitstatus_to_exitcode(status)
            reap_count+=1
            if len(reaped)<4096:reaped.append({'pid':pid,'exitCode':code,'at':time.monotonic(),'producer':pid == child_pid})
            if pid == child_pid:producer_exit=code;child.returncode=code
        try:
            rows=N.census();leftovers=[row for pid,row in rows.items() if pid != 1]
        except BaseException as error:
            if len(scan_errors)<8:scan_errors.append(repr(error))
            leftovers=[]
        if producer_exit is not None:
            if not leftovers and not scan_errors:
                closure.append({'at':time.monotonic(),'live':[]})
                if len(closure) == 2:break
            else:break  # Preserve failure/unknown before namespace teardown, never wait it into a pass.
        time.sleep(0.02)
    record={'kind':N.KIND,'profile':C.pin(profile_bytes),'producerExitCode':producer_exit,'producerPid':child_pid,
            'producerReceipt':None,'producerReceiptValidated':False,'beforeTeardown':True,'leftoverProcesses':leftovers,
            'scanErrors':scan_errors,'closureObservations':closure,'reaped':reaped,'initFinal':N.snapshot(),
            'reapCount':reap_count,'reapCoverageComplete':reap_count<=4096,'teardownAbsenceIsNotClosure':True}
    if reap_count>4096:record['scanErrors'].append('Bounded reap history incomplete')
    if producer_exit == 0:
        try:
            body=(output/profile['artifact']['receipt']).read_bytes();receipt=C.parse_json(body)
            C.validate_receipt(receipt,profile,C.pin(profile_bytes));record['producerReceipt']=C.pin(body);record['producerReceiptValidated']=True
            N.validate_snapshot(record['initFinal'],request['host'],request['uid'],request['gid'],init=True)
            N.validate_completion(record,C.pin(profile_bytes),producer_exit,C.pin(body),start)
        except BaseException as error:record['scanErrors'].append('completion: '+repr(error))
    try:write(context/'terminal.json',record)
    except BaseException as error:
        if producer_exit != 0:
            print('Secondary context capture failure; original producer exit remains '+repr(producer_exit)+': '+repr(error),file=sys.stderr,flush=True)
            return 1
        raise
    return 0 if producer_exit == 0 and record['producerReceiptValidated'] and not record['scanErrors'] and not leftovers and len(closure) == 2 else 1


if __name__ == '__main__':
    # Failure is durable in original producer/command receipts when present. PID1 teardown is separate.
    try:code=enter(C.parse_json(sys.argv[1].encode(),128*1024))
    except BaseException:
        raise
    raise SystemExit(code)
