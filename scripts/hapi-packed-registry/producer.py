"""Fixed Linux GitHub Actions producer. Never import or run this entry point on Windows.

Registry seed and OUTPUT evidence are separate; neither grants operational admission.
"""
import argparse
import base64
import gzip
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import platform
import resource
import shutil
import stat
import subprocess
import threading
import time
import tarfile
import contracts as C

HERE = Path(__file__).resolve().parent
TOOLS = HERE.parents[1]


def fresh(path, body):
    with Path(path).open('xb') as stream:
        stream.write(body)
        stream.flush()
        os.fsync(stream.fileno())
    return C.pin(body)


def save(path, value):
    return fresh(path, C.encoded(value))


def proc_rows():
    rows = {}
    for path in Path('/proc').iterdir():
        if not path.name.isdigit():
            continue
        try:
            raw = (path / 'stat').read_text(); fields = raw[raw.rindex(')') + 2:].split()
            rows[int(path.name)] = {'ppid':int(fields[1]),'group':int(fields[2]),'session':int(fields[3]),'birth':int(fields[19])}
        except FileNotFoundError:
            continue
    C.need(len(rows) <= 100000, 'Process census bound')
    return rows


def process_identity(pid):
    raw = (Path('/proc') / str(pid) / 'stat').read_text(); fields = raw[raw.rindex(')') + 2:].split()
    return {'ppid':int(fields[1]),'group':int(fields[2]),'session':int(fields[3]),'birth':int(fields[19])}


def resources():
    """Observations, not evidence that a hosted runner has the retained receiver limits."""
    usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    files = {}
    for name in ('cpu.max','cpu.stat','memory.max','memory.current','memory.swap.max','cpuset.cpus.effective'):
        path = Path('/sys/fs/cgroup') / name
        try:
            if path.is_file():
                with path.open('r') as stream:
                    body = stream.read(65537)
                files[name] = body if len(body) <= 65536 else {'unavailable':'observation exceeds bound'}
            else:
                files[name] = None
        except OSError as error:
            files[name] = {'unavailable':repr(error)}
    return {'monotonic':time.monotonic(),'cpuCount':os.cpu_count(),'childrenUserSeconds':usage.ru_utime,
            'childrenSystemSeconds':usage.ru_stime,'childrenMaxRssKiB':usage.ru_maxrss,'cgroupV2RootFiles':files}


def run(argv, cwd, env, folder, label, seconds, observer=None):
    """Original bounded wait/pipe/descendant observations; timeout never kills or relabels a child."""
    started = time.monotonic(); before = proc_rows(); streams, eof, errors = {}, set(), []
    buffers = {'stdout':bytearray(),'stderr':bytearray()}; counts = {'stdout':0,'stderr':0}; digests = {name:hashlib.sha256() for name in buffers}
    lock = threading.Lock(); intent = {'argv':argv,'cwd':str(cwd),'limitSeconds':seconds,'startedMonotonic':started,'resourcesBefore':resources()}
    save(folder / (label + '.intent.json'), intent)
    child = subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
    command = process_identity(child.pid)
    observed = {(child.pid,command['birth'])}; deadline = started + seconds

    def drain(name, pipe):
        try:
            with pipe:
                for body in iter(lambda:pipe.read(65536), b''):
                    with lock:
                        counts[name] += len(body); digests[name].update(body)
                        if len(buffers[name]) <= C.MAX_STREAM:
                            buffers[name].extend(body[:C.MAX_STREAM + 1 - len(buffers[name])])
            with lock:
                eof.add(name)
        except BaseException as error:
            with lock:
                errors.append(repr(error))

    readers = [threading.Thread(target=drain, args=(name,pipe), daemon=True) for name,pipe in [('stdout',child.stdout),('stderr',child.stderr)]]
    for thread in readers:
        thread.start()
    closure, code, snapshots = False, None, []
    while time.monotonic() < deadline:
        if observer is not None:
            try:
                observer()
            except BaseException as error:
                errors.append('observer: ' + repr(error)); observer = None
        try:
            census = proc_rows()
        except BaseException as error:
            errors.append('census: ' + repr(error)); code = child.poll(); break
        # Track all names, not only Node processes, including observed escaped descendants.
        changed = True
        while changed:
            changed = False
            for pid,row in census.items():
                parent = census.get(row['ppid'])
                if (row['session'] == child.pid or parent and (row['ppid'],parent['birth']) in observed) and (pid,row['birth']) not in observed:
                    observed.add((pid,row['birth'])); changed = True
        code = child.poll()
        with lock:
            pipes_closed = eof == {'stdout','stderr'}
        # Also reject an unobserved escaped child: no new live identity may outlast this command.
        live = [{'pid':pid,**row} for pid,row in census.items()
                if (pid,row['birth']) in observed or before.get(pid,{}).get('birth') != row['birth']]
        if code is not None and pipes_closed and not live:
            snapshots.append({'at':time.monotonic(),'live':live})
            if len(snapshots) == 2:
                closure = True; break
        else:
            snapshots = []
        time.sleep(0.02)
    with lock:
        for name in buffers:
            body = bytes(buffers[name]); streams[name] = {'bytes':counts[name],'sha256':digests[name].hexdigest(),'base64':base64.b64encode(body).decode(),'eof':name in eof}
    elapsed = time.monotonic() - started
    row = {**intent,'pid':child.pid,'birth':command['birth'],'processesBefore':before,'waitReaped':code is not None,'exitCode':code,
           'timedOut':elapsed >= seconds,'elapsedSeconds':elapsed,'streamsClosed':eof == {'stdout','stderr'},'descendantsClosed':closure,
           'closureObservations':snapshots,'observedProcesses':[{'pid':pid,'birth':birth}for pid,birth in sorted(observed)],'readerErrors':errors,
           'resourcesAfter':resources(),**streams}
    success = code == 0 and elapsed < seconds and closure and not errors and all(s['bytes'] <= C.MAX_STREAM and s['eof'] for s in streams.values())
    try:
        save(folder / (label + '.receipt.json'), row)
    except BaseException as secondary:
        failure = RuntimeError(('Original command failed/expired/unclosed: ' if not success else 'Original command completed but receipt capture failed: ') + label)
        failure.original_outcome = row; failure.add_note('Secondary receipt write failure: ' + repr(secondary)); raise failure from secondary
    C.need(success, 'Original command failed/expired/unclosed: ' + label)
    C.need(time.monotonic() < deadline, 'Command completed but evidence collection exceeded original cap: ' + label)
    return base64.b64decode(row['stdout']['base64']), row


def ordinary_inventory(root, logical_prefix):
    root = Path(root); rows = []
    def visit(path):
        info = path.lstat(); name = logical_prefix + ('/' + path.relative_to(root).as_posix() if path != root else '')
        if stat.S_ISDIR(info.st_mode):
            rows.append({'path':name,'mode':stat.S_IMODE(info.st_mode),'type':'dir'})
            for child in sorted(path.iterdir()):
                visit(child)
        else:
            C.need(stat.S_ISREG(info.st_mode) and info.st_nlink == 1 and info.st_size <= C.MAX_FILE, 'Ordinary runtime file required')
            rows.append({'path':name,'mode':stat.S_IMODE(info.st_mode),'type':'file',**C.pin(path.read_bytes())})
        C.need(len(rows) <= C.MAX_ENTRIES, 'Runtime member bound')
    visit(root); return rows


def runtime(node, npm, profile, expected):
    C.need(C.pin(node.read_bytes())['sha256'] == profile['runtime']['nodeSha256'], 'Actual Node executable differs')
    C.need(C.pin(npm.read_bytes()) == profile['runtime']['npmCli'], 'Actual npm CLI differs')
    npm_root = npm.parent.parent
    C.need(C.pin((npm_root / 'package.json').read_bytes()) == profile['runtime']['npmPackage'], 'Actual npm package differs')
    members = ordinary_inventory(npm_root, expected['prefix']); C.verify_runtime_members(members, expected['entries'])
    return npm_root, C.pin(C.encoded(sorted(members,key=lambda row:row['path'])))


def environment(work, node, npm_root, profile):
    env = dict(os.environ)
    for key in tuple(env):
        upper = key.upper()
        if key.lower().startswith('npm_config_') or upper.startswith('GIT_') or any(part in upper for part in ('TOKEN','SECRET','PASSWORD','PROXY')) or key in ('NODE_OPTIONS','NODE_PATH','NODE_EXTRA_CA_CERTS','SSL_CERT_FILE','SSL_CERT_DIR','SSH_AUTH_SOCK'):
            env.pop(key, None)
    for name in ('tmp','cache','config','home'):
        (work / name).mkdir(mode=0o700)
    for name in ('user.npmrc','global.npmrc','global.gitconfig'):
        fresh(work / name, b'# task-owned empty engineering configuration\n')
    env.update(PATH=str(node.parent) + os.pathsep + env.get('PATH',''),
        npm_config_userconfig=str(work/'user.npmrc'),npm_config_globalconfig=str(work/'global.npmrc'),
        npm_config_cache=str(work/'cache/npm'),npm_config_registry=profile['registry'],
        npm_config_audit='false',npm_config_fund='false',npm_config_update_notifier='false',HOME=str(work/'home'),
        GIT_CONFIG_GLOBAL=str(work/'global.gitconfig'),GIT_CONFIG_NOSYSTEM='1',GIT_TERMINAL_PROMPT='0',
        XDG_CACHE_HOME=str(work/'cache'),XDG_CONFIG_HOME=str(work/'config'),TMPDIR=str(work/'tmp'),
        REGISTRY94_NPM_ROOT=str(npm_root),**profile['sourceEnvironment'])
    return env


def write_archive(root, rows, path):
    with (path.open('xb') as target, gzip.GzipFile(fileobj=target, mode='wb', mtime=0, filename='') as compressed,
          tarfile.open(fileobj=compressed, mode='w', format=tarfile.PAX_FORMAT) as archive):
        for row in sorted(rows,key=lambda row:(row['path'].count('/'),row['path'])):
            member = tarfile.TarInfo(row['path']); member.mode = row['mode']; member.uid = member.gid = member.mtime = 0
            if row['type'] == 'directory':
                member.type = tarfile.DIRTYPE; archive.addfile(member)
            else:
                body = (root / row['path']).read_bytes(); C.need(C.pin(body) == {k:row[k]for k in ('bytes','sha256')}, 'Seed changed before seal')
                member.size = len(body); archive.addfile(member, io.BytesIO(body))
    C.verify_archive(path, rows)


def evidence_inventory(root):
    rows, total = [], 0
    for path in sorted(root.rglob('*')):
        name = C.safe_path(path.relative_to(root).as_posix()); info = path.lstat(); mode = stat.S_IMODE(info.st_mode)
        if stat.S_ISLNK(info.st_mode):
            target = os.readlink(path); C.need((path.parent / target).resolve().is_relative_to(root.resolve()), 'Installed graph link escapes owned tree')
            rows.append({'path':name,'type':'symlink','mode':mode,'target':target})
        elif stat.S_ISDIR(info.st_mode):
            rows.append({'path':name,'type':'directory','mode':mode})
        else:
            C.need(stat.S_ISREG(info.st_mode) and info.st_nlink == 1 and info.st_size <= C.MAX_FILE, 'Ordinary evidence file required')
            total += info.st_size; C.need(total <= C.MAX_PAYLOAD, 'Evidence payload bound')
            rows.append({'path':name,'type':'file','mode':mode,**C.pin(path.read_bytes())})
        C.need(len(rows) <= C.MAX_ENTRIES, 'Evidence inventory bound')
    return rows


def produce(repo, output, case='v1'):
    C.need(platform.system() == 'Linux' and platform.machine() == 'x86_64', 'Linux/x64 CI entry point only')
    C.need(os.environ.get('GITHUB_ACTIONS') == 'true' and os.environ.get('GITHUB_REPOSITORY') == 'eosrio/hyperion-tools', 'Actual Tools CI required')
    profile_bytes = (HERE/('profile-pidns.json' if case == 'pidns-v2' else 'profile.json')).read_bytes(); profile, expected = C.load_contract(profile_bytes, (HERE/'npm-members.json').read_bytes())
    C.need(os.environ.get('GITHUB_JOB') == profile['job'] and os.environ.get('GITHUB_WORKFLOW_REF','').startswith('eosrio/hyperion-tools/' + profile['workflow'] + '@'), 'Actual selected workflow/job required')
    context_start = context_before = None
    if case == 'pidns-v2':
        import namespace_contracts as N
        import pidns_runner as P
        N.source_pins(profile,HERE)
        context_start,context_before=N.current_context(os.environ['REGISTRY_PIDNS_CONTEXT'],profile)
        C.need(os.environ.get('REGISTRY_PROFILE') == 'pidns-v2', 'Explicit isolated profile selection required')
    git = lambda path,*args:subprocess.check_output(['git','-C',str(path),*args],stderr=subprocess.PIPE).decode().strip()
    revision, tree = git(TOOLS,'rev-parse','HEAD'), git(TOOLS,'rev-parse','HEAD^{tree}')
    C.need(revision == os.environ['GITHUB_WORKFLOW_SHA'], 'Actual workflow source revision differs')
    identity = {'repository':'eosrio/hyperion-tools','workflowPath':profile['workflow'],'workflowRevision':revision,'workflowTree':tree,
                'runId':int(os.environ['GITHUB_RUN_ID']),'runAttempt':int(os.environ['GITHUB_RUN_ATTEMPT']),'job':profile['job']}
    node = Path(shutil.which('node')).resolve(strict=True); npm = Path(shutil.which('npm')).resolve(strict=True)
    C.need(node.name == 'node' and npm.name == 'npm-cli.js', 'Actual runtime entry points required')
    npm_root, runtime_before = runtime(node, npm, profile, expected)
    repo, output = repo.resolve(strict=True), output.absolute()
    C.fresh_target(output.parent.resolve(strict=True), output)
    C.need(not output.is_relative_to(repo) and not output.is_relative_to(TOOLS), 'External owned artifact workspace required')
    C.need(shutil.disk_usage(output.parent).free >= profile['retainedReceiverLimits']['reserveBytes'], 'Original45GiB reserve unavailable; no cleanup/waiver')
    old_path = TOOLS/'scripts/hapi-linux-dependencies/producer.py'; old_bytes = old_path.read_bytes()
    C.need(C.pin(old_bytes) == {'bytes':32453,'sha256':'bf36ec5137cac4703e4af18f22585cbc9bf39a86e80375585cca828a2b41b8a3'}, 'Frozen current103 source validator differs')
    module_spec = importlib.util.spec_from_file_location('frozen_current103',old_path); old = importlib.util.module_from_spec(module_spec); module_spec.loader.exec_module(old)
    old.verify_source(repo, profile['product'])
    output.mkdir(mode=0o700); work = output/'work'; work.mkdir(mode=0o700); commands = work/'commands'; commands.mkdir(mode=0o700)
    env = environment(work,node,npm_root,profile); commands_seen = []
    def call(argv,cwd,label,seconds=120,selected_env=None,observer=None):
        args=([str(x)for x in argv],cwd,selected_env or env,commands,label,seconds,observer)
        data,row = (P.run(*args,profile=profile,context_path=os.environ['REGISTRY_PIDNS_CONTEXT'],resources=resources)
                    if case == 'pidns-v2' else run(*args))
        commands_seen.append(row); return data,row
    def reserve():
        C.need(shutil.disk_usage(output).free >= profile['retainedReceiverLimits']['reserveBytes'], 'Original45GiB reserve crossed')
    version,_ = call([node,'--version'],repo,'node-version',15); C.need(version.decode().strip() == profile['runtime']['node'], 'Actual Node version differs')
    version,_ = call([node,npm,'--version'],repo,'npm-version',15); C.need(version.decode().strip() == profile['runtime']['npm'], 'Actual npm version differs')
    version,_ = call(['bun','--version'],repo,'bun-version',15); C.need(version.decode().strip() == '1.4.4', 'Pinned Bun checker required')
    version,_ = call(['bun','-p','process.versions.typescript'],repo,'typescript-version',15); C.need(version.decode().strip() == '7.0.2', 'Pinned TS checker required')
    provenance = {}
    def prepare(path,label):
        for member in ('node_modules','build','packages/backend-mongodb/node_modules','packages/backend-mongodb/dist','.npmrc'):
            C.need(not os.path.lexists(path/member), 'Fresh source/dependency/build target required: ' + member)
        old.verify_source(path,profile['product']); cache = work/(label+'-install-cache'); cache.mkdir(mode=0o700)
        selected = {**env,'npm_config_cache':str(cache)}
        call([node,npm,'ci','--allow-git=all','--ignore-scripts=false'],path,label+'-npm-root',1200,selected)
        call([node,npm,'ci','--allow-git=all','--ignore-scripts=false','--prefix','packages/backend-mongodb'],path,label+'-npm-native',1200,selected)
        old.verify_source(path,profile['product']); rows = old.dependency_inventory(path)
        provenance[label] = old.dependency_provenance(path,rows,profile['product'],old.CASES['current103']['preparedSdk'])
        C.need(json.loads((path/'node_modules/typescript/package.json').read_bytes())['version'] == '7.0.2', 'Actual TS compiler differs')
        call(['bun','check','-p','tsconfig.json'],path,label+'-whole-check',60,selected)
        call(['bun','check','-p','packages/backend-mongodb/tsconfig.json'],path,label+'-adapter-check',60,selected)
        call([node,npm,'run','build','--prefix','packages/backend-mongodb'],path,label+'-native-build',120,selected)
        call([node,path/'node_modules/typescript/bin/tsc','-p','tsconfig.json'],path,label+'-root-build',120,selected)
        emitted = [*[{**row,'path':'build/'+row['path']}for row in evidence_inventory(path/'build')],
                   *[{**row,'path':'packages/backend-mongodb/dist/'+row['path']}for row in evidence_inventory(path/'packages/backend-mongodb/dist')]]
        C.need(sum(row['type'] == 'file' for row in emitted) == 589, 'Original589 fresh emissions required')
        old.verify_source(path,profile['product']); reserve(); return selected,emitted
    normal_env, normal_emissions = prepare(repo,'resolver-source')
    resolver = work/'resolver'; resolver.mkdir(mode=0o700); packs = resolver/'packs'; packs.mkdir(mode=0o700); pack_cache = resolver/'pack-cache'; pack_cache.mkdir(mode=0o700)
    for label,path in [('adapter',repo/'packages/backend-mongodb'),('sdk',repo/'packages/backend-mongodb/node_modules/@eosrio/hyperion-sdk')]:
        call([node,npm,'pack','--ignore-scripts','--pack-destination',packs],path,'resolver-pack-'+label,60,{**env,'npm_config_cache':str(pack_cache)})
    tarballs = sorted(packs.glob('*.tgz')); C.need(len(tarballs) == 2, 'Original two fresh local packs required')
    consumer = resolver/'consumer'; consumer.mkdir(mode=0o700); fresh(consumer/'package.json',b'{"name":"stateful-native-consumer","private":true,"type":"module"}')
    resolver_cache = resolver/'npm-cache'; resolver_cache.mkdir(mode=0o700)
    call([node,npm,'install','--ignore-scripts','--no-audit','--no-fund',*tarballs,'@types/node@25.5.0'],consumer,'normal-resolver',60,{**env,'npm_config_cache':str(resolver_cache)})
    lock_bytes = (consumer/'package-lock.json').read_bytes()
    # Retain original resolver bytes even when parsing/graph admission refuses.
    fresh(output/'normal-resolver.package-lock.json',lock_bytes)
    fresh(output/'normal-resolver.package.json',(consumer/'package.json').read_bytes())
    lock = C.parse_json(lock_bytes); packages,requests = C.graph(lock,profile)
    installed = evidence_inventory(consumer/'node_modules'); closure = resolver/'closure.json'
    save(closure,{'packages':packages,'requests':{name:sorted(values)for name,values in requests.items()}})
    export_spec = resolver/'export.json'; save(export_spec,{'cacheRoot':str(resolver_cache),'output':str(work/'export'),'closure':str(closure),'consumer':str(consumer),'localPacks':[str(p)for p in tarballs]})
    call([node,HERE/'cache-bridge.mjs','export',export_spec],TOOLS,'export-registry',120)
    seed = work/'export/cache'; catalog = C.parse_json((work/'export/catalog.json').read_bytes()); cache = C.validate_cache_tree(seed,catalog['entries'],lock,profile)
    archive = output/profile['artifact']['archive']; write_archive(seed,cache['inventory'],archive); archive_pin = C.pin(archive.read_bytes())
    proof_root = work/'proof-product'; call(['git','clone','--no-local','--no-hardlinks','--no-checkout',repo,proof_root],work,'fresh-proof-clone',120)
    call(['git','checkout','--detach',profile['product']['productHead']],proof_root,'fresh-proof-checkout',60)
    proof_env, proof_emissions = prepare(proof_root,'proof-source')
    mutable = work/'proof-cache'; C.fresh_target(work,mutable); shutil.copytree(seed,mutable,copy_function=shutil.copyfile)
    # copyfile does not preserve read-only source modes; restore and verify every exact input before npm.
    for row in cache['inventory']:
        (mutable/row['path']).chmod(row['mode'])
    C.need(C.validate_cache_tree(mutable,catalog['entries'],lock,profile)['inventory'] == cache['inventory'], 'Fresh mutable cache differs from sealed seed')
    proof_tmp = work/'proof-tmp'; proof_tmp.mkdir(mode=0o700); observed = {}
    def observe():
        for path in proof_tmp.iterdir():
            if path.name.startswith('stateful-mongodb-consumer-'):
                try:
                    info = path.lstat()
                except FileNotFoundError:
                    continue
                C.need(stat.S_ISDIR(info.st_mode) and not path.is_symlink(), 'Original consumer tmp identity differs')
                previous = observed.setdefault(str(path),{'device':info.st_dev,'inode':info.st_ino})
                C.need(previous == {'device':info.st_dev,'inode':info.st_ino}, 'Consumer tmp identity reused')
    actual_env = {**proof_env,**profile['offlineEnvironment'],'npm_config_cache':str(mutable),'TMPDIR':str(proof_tmp)}
    reserve(); _,terminal = call([node,npm,'run','test:package','--prefix','packages/backend-mongodb'],proof_root,'unchanged-offline-packed',60,actual_env,observe)
    C.validate_terminal(terminal); C.need(len(observed) == 1, 'Actual original consumer tmp observation missing/ambiguous')
    original_consumer = Path(next(iter(observed)))/'consumer'
    output_spec = work/'output-spec.json'; output_dir = output/'OUTPUT'
    save(output_spec,{'role':'OUTPUT','cacheRoot':str(mutable),'output':str(output_dir),'consumer':str(original_consumer),'proofTmp':str(proof_tmp)})
    call([node,HERE/'cache-bridge.mjs','outputs',output_spec],TOOLS,'actual-pack-output-custody',60,actual_env)
    actual_packs = C.parse_json((output_dir/'packs.json').read_bytes()); outputs = []
    sources = {C.LOCAL_NAMES[0]:proof_root/'packages/backend-mongodb',C.LOCAL_NAMES[1]:proof_root/'packages/backend-mongodb/node_modules/@eosrio/hyperion-sdk'}
    for entry in actual_packs:
        body = (output_dir/entry['path']).read_bytes(); parsed = C.package_tar(body); name = parsed['manifest']['name']; C.need(name in sources, 'Foreign actual local output pack')
        root = parsed['root'] + '/'
        for member in parsed['members']:
            if member['type'] == 'file':
                relative = member['path'].removeprefix(root); source = sources[name]/relative
                C.need(source.is_file() and not source.is_symlink() and C.pin(source.read_bytes()) == {k:member[k]for k in ('bytes','sha256')}, 'Actual selected pack differs from fresh source/SDK/build')
        outputs.append({**entry,'name':name,'version':parsed['manifest']['version'],'members':parsed['members'],'freshSourceRoot':str(sources[name])})
    C.need({r['name']for r in outputs} == set(C.LOCAL_NAMES), 'Both actual output identities required')
    output_rows = C.cache_inventory(output_dir); write_archive(output_dir,output_rows,output/profile['artifact']['outputs'])
    C.need(C.pin(archive.read_bytes()) == archive_pin and C.validate_cache_tree(seed,catalog['entries'],lock,profile)['inventory'] == cache['inventory'], 'Sealed seed changed during proof')
    old.verify_source(repo,profile['product']); old.verify_source(proof_root,profile['product']); _,runtime_after = runtime(node,npm,profile,expected); C.need(runtime_after == runtime_before, 'Runtime changed during producer')
    reserve(); receipt = {'schemaVersion':profile['schemaVersion'],'kind':profile['kind'],'profile':C.pin(profile_bytes),'product':profile['product'],'consumer':profile['consumer'],
        'producer':identity,'runtime':profile['runtime'],'runtimeMembersBefore':runtime_before,'runtimeMembersAfter':runtime_after,'offlineEnvironment':profile['offlineEnvironment'],
        'scope':{'isolatedPackedConsumer':True,'fullWireComponent':'NOT_RUN','receiverLimitsUnchanged':profile['retainedReceiverLimits']},
        'seedSealedBeforeProof':True,'seedContainsLocalPacksOrBuilds':False,'freshProofSourceAndBuild':True,'proof':terminal,'actualOutputPacks':outputs,
        'registry':{'consumerLock':lock,'consumerLockPin':C.pin(lock_bytes),'installedGraph':installed,'catalog':catalog,'validatedCatalog':cache['entries'],'inventory':cache['inventory']},
        'archive':archive_pin,'outputsArchive':C.pin((output/profile['artifact']['outputs']).read_bytes()),'freshEmissions':{'resolver':normal_emissions,'proof':proof_emissions},
        'dependencyProvenance':provenance,'commands':commands_seen,'consumerTmp':observed,'postUploadArtifact':None}
    if case == 'pidns-v2':
        _,context_after=N.current_context(os.environ['REGISTRY_PIDNS_CONTEXT'],profile)
        receipt['executionContext']={'kind':N.KIND,'contextPin':context_before['contextPin'],'start':context_start,'before':context_before,'after':context_after}
        receipt['commandLayout']={'productRoot':str(repo),'workRoot':str(work),'toolsRoot':str(TOOLS),'node':str(node),'npm':str(npm)}
    C.validate_receipt(receipt,profile,C.pin(profile_bytes)); body = C.encoded(receipt); C.need(len(body) <= C.MAX_RECEIPT, 'Receipt16MiB bound'); fresh(output/profile['artifact']['receipt'],body)
    print(json.dumps({'artifact':profile['artifact']['name'],'receipt':C.pin(body),'archive':archive_pin,'fullWireComponent':'NOT_RUN'}),flush=True)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(); parser.add_argument('--profile',choices=['v1','pidns-v2'],default='v1'); parser.add_argument('--product-root',type=Path,required=True); parser.add_argument('--output',type=Path,required=True); args = parser.parse_args()
    try:
        produce(args.product_root,args.output,args.profile)
    except BaseException as error:
        if args.output.is_dir():
            try:
                save(args.output/'FAILED-PRODUCER.json',{'eligible':False,'error':repr(error),'fullWireComponent':'NOT_RUN','originalFailedReceiptsRemainInWork':True})
            except BaseException as secondary:
                error.add_note('Secondary failure-receipt error: ' + repr(secondary))
        raise
