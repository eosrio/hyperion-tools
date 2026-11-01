"""Linux v2 command runner. Import is inert; never execute on Windows."""
import base64
import hashlib
import os
from pathlib import Path
import stat
import subprocess
import threading
import time
import contracts as C
import namespace_contracts as N


def fresh(path, value):
    with path.open('xb') as stream:
        stream.write(C.encoded(value));stream.flush();os.fsync(stream.fileno())


def diagnostic(path, value):
    # Only this bounded diagnostic slot is mutable. It never replaces a terminal receipt.
    if path.exists():
        info=path.lstat();C.need(stat.S_ISREG(info.st_mode) and info.st_nlink == 1, 'Ordinary diagnostic slot required')
    temporary=path.with_suffix('.next')
    fresh(temporary,value);os.replace(temporary,path)


def run(argv,cwd,env,folder,label,seconds,observer=None,*,profile,context_path,resources):
    started=time.monotonic();deadline=started+seconds
    N.source_pins(profile,Path(__file__).resolve().parent)
    context,before_context=N.current_context(context_path,profile);before=N.census()
    intent={'argv':argv,'cwd':str(cwd),'limitSeconds':seconds,'startedMonotonic':started,
            'resourcesBefore':resources(),'namespaceBefore':before_context,'closurePolicy':N.KIND}
    fresh(folder/(label+'.intent.json'),intent)
    fresh(folder/(label+'.start.json'),{'event':'start','at':time.monotonic(),'namespace':before_context,'census':list(before.values()),'complete':True})
    print(C.encoded({'event':'command-start','label':label,'limitSeconds':seconds,'at':started}).decode().strip(),flush=True)
    child=subprocess.Popen(argv,cwd=cwd,env=env,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
    command=N.parse_stat(child.pid,N.read_text(Path('/proc')/str(child.pid)/'stat',4096));observed={(child.pid,command['birth'])}
    buffers={name:bytearray()for name in ('stdout','stderr')};counts={name:0 for name in buffers};digests={name:hashlib.sha256()for name in buffers}
    eof={};reader_errors=[];secondary=[];lock=threading.Lock()
    def drain(name,pipe):
        try:
            with pipe:
                for body in iter(lambda:pipe.read(65536),b''):
                    with lock:
                        counts[name]+=len(body);digests[name].update(body)
                        if len(buffers[name])<=C.MAX_STREAM:buffers[name].extend(body[:C.MAX_STREAM+1-len(buffers[name])])
            with lock:eof[name]=time.monotonic()
        except BaseException as error:
            with lock:reader_errors.append(repr(error))
    for name,pipe in [('stdout',child.stdout),('stderr',child.stderr)]:threading.Thread(target=drain,args=(name,pipe),daemon=True).start()
    code=None;direct_exit=None;closures=[];last_complete=None;last_blocking=None;last_write=started;complete=False;eof_recorded=False
    while time.monotonic()<deadline:
        if observer is not None:
            try:observer()
            except BaseException as error:secondary.append('observer: '+repr(error));observer=None
        try:
            rows=N.census();observed,live=N.classify(before,rows,observed,child.pid)
            last_complete={'at':time.monotonic(),'complete':True,'namespace':before_context['actual']['pidNamespace'],
                           'census':list(rows.values()),'live':live}
        except BaseException as error:
            secondary.append('census: '+repr(error));code=child.poll();break
        code=child.poll()
        if code is not None and direct_exit is None:
            direct_exit={'at':time.monotonic(),'exitCode':code,'waitReaped':True}
            try:fresh(folder/(label+'.direct-exit.json'),direct_exit)
            except BaseException as error:secondary.append('direct-exit diagnostic: '+repr(error))
        with lock:pipes_closed=set(eof)=={'stdout','stderr'};current_eof=dict(eof)
        last_complete.update(directExit=direct_exit,pipeEofTimes=current_eof)
        if pipes_closed and not eof_recorded:
            try:fresh(folder/(label+'.eof.json'),{'event':'pipe-eof','times':current_eof,'bothClosed':True})
            except BaseException as error:secondary.append('EOF diagnostic: '+repr(error))
            eof_recorded=True
        if live:last_blocking=last_complete
        if time.monotonic()-last_write>=5:
            try:diagnostic(folder/(label+'.last-census.json'),last_complete)
            except BaseException as error:secondary.append('last-census diagnostic: '+repr(error))
            last_write=time.monotonic()
        if code is not None and pipes_closed and not live:
            closures.append({'at':time.monotonic(),'live':[],'complete':True})
            if len(closures)==2:complete=True;break
        else:closures=[]
        if len(secondary)>8:secondary=secondary[:8]+['Additional diagnostic failures censored; command cannot pass']
        time.sleep(0.02)
    streams={}
    with lock:
        for name in buffers:
            body=bytes(buffers[name]);streams[name]={'bytes':counts[name],'sha256':digests[name].hexdigest(),
                'base64':base64.b64encode(body).decode(),'eof':name in eof}
        eof_events=dict(eof)
    try:
        N.source_pins(profile,Path(__file__).resolve().parent)
        _,after_context=N.current_context(context_path,profile)
    except BaseException as error:after_context=None;secondary.append('namespace: '+repr(error))
    elapsed=time.monotonic()-started
    row={**intent,'pid':child.pid,'birth':command['birth'],'processesBefore':before,'waitReaped':code is not None,
         'exitCode':code,'timedOut':elapsed>=seconds,'elapsedSeconds':elapsed,'streamsClosed':set(eof_events)=={'stdout','stderr'},
         'descendantsClosed':complete,'closureObservations':closures,'observedProcesses':[{'pid':p,'birth':b}for p,b in sorted(observed)],
         'readerErrors':reader_errors,'diagnosticErrors':secondary,'directExit':direct_exit,'pipeEofTimes':eof_events,
         'lastCompleteCensus':last_complete,'lastBlockingCensus':last_blocking,'namespaceAfter':after_context,
         'resourcesAfter':resources(),**streams}
    success=code==0 and complete and elapsed<seconds and not reader_errors and not secondary and after_context is not None
    success=success and all(value['eof'] and value['bytes']<=C.MAX_STREAM for value in streams.values())
    try:
        diagnostic(folder/(label+'.last-census.json'),last_complete or {'complete':False,'error':'No complete census'})
        if not eof_recorded:fresh(folder/(label+'.eof.json'),{'event':'pipe-eof','times':eof_events,'bothClosed':row['streamsClosed']})
        fresh(folder/(label+'.receipt.json'),row)
    except BaseException as error:
        failure=RuntimeError('Original command failed/expired/unclosed: '+label if not success else 'Original command completed but capture failed: '+label)
        failure.original_outcome=row;failure.add_note('Secondary capture failure: '+repr(error));raise failure from error
    print(C.encoded({'event':'command-terminal','label':label,'elapsedSeconds':elapsed,'exitCode':code,
                     'streamsClosed':row['streamsClosed'],'descendantsClosed':complete,'timedOut':row['timedOut']}).decode().strip(),flush=True)
    C.need(success,'Original command failed/expired/unclosed: '+label)
    C.need(time.monotonic()<deadline,'Evidence capture exceeded original command cap: '+label)
    return base64.b64decode(row['stdout']['base64']),row
