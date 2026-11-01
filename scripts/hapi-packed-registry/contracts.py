"""Read-only data validators. Importing this module never starts a producer or npm."""
import base64
import gzip
import hashlib
import io
import json
import math
from pathlib import Path, PurePosixPath
import re
import stat
import tarfile
from urllib.parse import unquote, urlsplit

MAX_ENTRIES, MAX_PAYLOAD, MAX_FILE = 30000, 2 * 1024**3, 256 * 1024**2
MAX_ARCHIVE, MAX_RECEIPT, MAX_STREAM = 1024**3, 16 * 1024**2, 2 * 1024**2
REGISTRY = 'https://registry.npmjs.org/'
LOCAL_NAMES = ('@eosrio/hyperion-backend-mongodb', '@eosrio/hyperion-sdk')
REQUEST_HEADERS = {'accept-charset','accept-encoding','accept-language','accept','cache-control','user-agent'}
RESPONSE_HEADERS = {'cache-control','content-encoding','content-language','content-type','date','etag','expires','last-modified','link','location','pragma','vary','npm-notice'}


def need(value, message):
    if not value:
        raise ValueError(message)


def encoded(value):
    return (json.dumps(value, indent=2) + '\n').encode()


def pin(body):
    return {'bytes': len(body), 'sha256': hashlib.sha256(body).hexdigest()}


def _pairs(values):
    result = {}
    for key, value in values:
        need(key not in result, 'Duplicate JSON key')
        result[key] = value
    return result


def parse_json(body, maximum=MAX_RECEIPT):
    need(type(body) is bytes and 0 < len(body) <= maximum, 'Bounded JSON bytes required')
    return json.loads(body.decode('utf-8'), object_pairs_hook=_pairs,
                      parse_constant=lambda _: (_ for _ in ()).throw(ValueError('Nonfinite JSON')))


def safe_path(name):
    need(type(name) is str and 0 < len(name.encode()) <= 4096 and not name.startswith('/'), 'Unsafe path')
    need(not any(c in name for c in ('\\', ':', '\0')) and all(31 < ord(c) != 127 for c in name), 'Unsafe path text')
    need(all(part not in ('', '.', '..') for part in name.split('/')), 'Noncanonical path')
    return name


def sri(value):
    need(type(value) is str and value.startswith('sha512-') and len(value) == 95, 'Exact sha512 SRI required')
    try:
        raw = base64.b64decode(value[7:], validate=True)
    except Exception as error:
        raise ValueError('Invalid SRI') from error
    need(len(raw) == 64 and base64.b64encode(raw).decode() == value[7:], 'Canonical SRI required')
    return raw


def body_sri(body):
    return 'sha512-' + base64.b64encode(hashlib.sha512(body).digest()).decode()


def content_path(integrity):
    value = sri(integrity).hex()
    return '_cacache/content-v2/sha512/' + value[:2] + '/' + value[2:4] + '/' + value[4:]


def index_path(key):
    value = hashlib.sha256(key.encode()).hexdigest()
    return '_cacache/index-v5/' + value[:2] + '/' + value[2:4] + '/' + value[4:]


def registry_url(url):
    need(type(url) is str and len(url) <= 4096, 'Bounded registry URL required')
    parsed = urlsplit(url)
    need(parsed.scheme == 'https' and parsed.netloc == 'registry.npmjs.org'
         and not parsed.query and not parsed.fragment and url.startswith(REGISTRY), 'Only public fixed registry URL admitted')
    decoded = unquote(parsed.path[1:])
    safe_path(decoded)
    return decoded


def package_name(value):
    need(type(value) is str and re.fullmatch(r'(?:@[a-z0-9][a-z0-9._-]*/)?[a-z0-9][a-z0-9._-]*', value), 'Canonical registry package name required')
    return value


def location_name(location):
    safe_path(location)
    parts = location.split('/'); position = 0; name = None
    while position < len(parts):
        need(parts[position] == 'node_modules' and position + 1 < len(parts), 'Only installed package locations admitted')
        position += 1; name = parts[position]; position += 1
        if name.startswith('@'):
            need(position < len(parts), 'Incomplete scoped location')
            name += '/' + parts[position]; position += 1
        package_name(name)
    return name


def mappings(row, field):
    result = row.get(field, {})
    need(type(result) is dict and len(result) <= 1024 and all(type(v) is str and 0 < len(v) <= 2048 for v in result.values()), 'Bounded dependency mapping required')
    for name in result:
        package_name(name)
    return result


def graph(lock, profile):
    """Recorded npm resolution, with complete reachability; not a replacement semver solver."""
    need(type(lock) is dict and lock.get('lockfileVersion') == 3 and lock.get('name') == 'stateful-native-consumer', 'Actual unlocked consumer lock required')
    packages = lock.get('packages'); need(type(packages) is dict and 3 < len(packages) <= 1024 and '' in packages, 'Complete bounded consumer graph required')
    root = mappings(packages[''], 'dependencies')
    # npm11.17 saves an explicit version with its default '^' prefix. This is
    # saved metadata, not permission to change the original CLI selection.
    need(set(root) == set(LOCAL_NAMES) | {'@types/node'} and root['@types/node'] in ('25.5.0','^25.5.0'), 'Original three consumer arguments required')
    by, requests = {}, {'@types/node': {'25.5.0'}}  # Original unchanged CLI argument.
    for location, row in packages.items():
        if location == '':
            continue
        name = location_name(location)
        need(type(row) is dict and row.get('link') is not True and type(row.get('version')) is str and row.get('name', name) == name, 'Resolved ordinary package required')
        sri(row.get('integrity'))
        if name in LOCAL_NAMES:
            need(location == 'node_modules/' + name and row['version'] == '0.0.0-dev.1'
                 and type(row.get('resolved')) is str and row['resolved'].startswith('file:') and row['resolved'].endswith('.tgz'), 'Only the two fresh local source packs allowed')
        else:
            need(registry_url(row.get('resolved')).startswith(name + '/-/'), 'Package tarball URL/name differs')
        by[location] = {'name': name, **row}
    need(all('node_modules/' + name in by for name in LOCAL_NAMES), 'Both original local packages required')
    need(by.get('node_modules/@types/node', {}).get('version') == '25.5.0', 'Original exact root @types/node selection required')
    for field, value in [('mongodb','7.2.0'),('ajv','8.20.0'),('semver','7.8.5'),('@types/node','25.5.0')]:
        need(any(row['name'] == field and row['version'] == value for row in by.values()), 'Fixed direct registry root absent')
    need(mappings(by['node_modules/' + LOCAL_NAMES[0]], 'dependencies') == {'mongodb':'7.2.0'}
         and mappings(by['node_modules/' + LOCAL_NAMES[1]], 'dependencies') == {'ajv':'8.20.0','semver':'7.8.5'}, 'Source package dependency roots differ')

    def find(origin, name):
        prefix = origin
        while prefix:
            candidate = prefix + '/node_modules/' + name
            if candidate in by:
                return candidate
            prefix = prefix.rsplit('/node_modules/', 1)[0] if '/node_modules/' in prefix else ''
        candidate = 'node_modules/' + name
        return candidate if candidate in by else None

    pending = [('', root)]; reached = set()
    while pending:
        origin, dependencies = pending.pop()
        parent = packages[origin]
        optional = set(mappings(parent, 'optionalDependencies'))
        peers = parent.get('peerDependenciesMeta', {})
        need(type(peers) is dict, 'Peer optional metadata required')
        optional.update(name for name, data in peers.items() if type(data) is dict and data.get('optional') is True)
        for name, spec in dependencies.items():
            requests.setdefault(name, set()).add(spec)
            target = find(origin, name)
            need(target is not None or name in optional, 'Missing required resolved dependency')
            if target is None or target in reached:
                continue
            reached.add(target); row = by[target]
            combined = {}
            for field in ('dependencies','optionalDependencies','peerDependencies'):
                combined.update(mappings(row, field))
            pending.append((target, combined))
    need(reached == set(by), 'Unreferenced package payload in resolution')
    return by, requests


def _metadata(value, url):
    need(type(value) is dict and set(value) == {'time','url','reqHeaders','resHeaders','options'}, 'Exact public HTTP cache metadata required')
    need(type(value['time']) is int and value['time'] > 0 and value['url'] == url, 'HTTP response identity/time differs')
    need(value['options'] in ({'compress':True},{'compress':False}), 'Only original compression option admitted')
    for key, allowed in [('reqHeaders', REQUEST_HEADERS), ('resHeaders', RESPONSE_HEADERS)]:
        headers = value[key]
        need(type(headers) is dict and set(headers) <= allowed, 'Unexpected/auth/cookie cache header refused')
        need(all(type(v) is str and len(v.encode()) <= 16384 and '\r' not in v and '\n' not in v for v in headers.values()), 'Invalid cache header value')


def classify_key(key, packages, requests):
    need(type(key) is str and 0 < len(key.encode()) <= 8192, 'Bounded exact cache key required')
    rows = [row for row in packages.values() if row['name'] not in LOCAL_NAMES]
    urls = {row['resolved'] for row in rows}; names = {row['name'] for row in rows}
    metadata_names = set(requests) - set(LOCAL_NAMES)
    if key.startswith('make-fetch-happen:request-cache:'):
        url = key[len('make-fetch-happen:request-cache:'):]; name = registry_url(url)
        if url in urls:
            return 'tarball', url
        need(name in metadata_names, 'Unreferenced registry metadata key')
        return 'metadata', name
    need(key.startswith('pacote:tarball:'), 'Unexpected cache namespace')
    spec = key[len('pacote:tarball:'):]
    if spec in urls:
        return 'tarball', spec
    match = re.fullmatch(r'(@[^/]+/[^@]+|[^@]+)@(.+)', spec)
    need(match and match[1] in names and match[2] in requests.get(match[1], set()), 'Local/Git/unreferenced pacote tarball refused')
    return 'package-tarball', match[1]


def package_tar(body):
    need(type(body) is bytes and 0 < len(body) <= MAX_FILE, 'Bounded registry tarball required')
    rows, seen, total, roots, manifest = [], set(), 0, set(), None
    with gzip.GzipFile(fileobj=io.BytesIO(body)) as raw, tarfile.open(fileobj=BoundedRead(raw), mode='r:') as archive:
        for member in archive:
            name = member.name[:-1] if member.isdir() and member.name.endswith('/') else member.name
            safe_path(name); need(name not in seen and len(seen) < MAX_ENTRIES, 'Duplicate/excess package tar member')
            seen.add(name); roots.add(name.split('/')[0])
            need(not member.sparse and set(member.pax_headers) <= {'path'}, 'Unsupported sparse/PAX package metadata')
            need(member.mode in (0o644,0o755), 'Unsafe package member mode')
            if member.isdir():
                need(member.size == 0, 'Nonempty directory payload')
                rows.append({'path':name,'type':'directory','mode':member.mode})
            else:
                need(member.type in (tarfile.REGTYPE,tarfile.AREGTYPE) and 0 <= member.size <= MAX_FILE, 'Package links/devices/oversized files refused')
                total += member.size; need(total <= MAX_PAYLOAD, 'Package expanded bound')
                stream = archive.extractfile(member); checksum = hashlib.sha256(); count = 0; collected = []
                for chunk in iter(lambda:stream.read(1024**2), b''):
                    checksum.update(chunk); count += len(chunk)
                    if name.endswith('/package.json') and len(name.split('/')) == 2:
                        need(count <= MAX_RECEIPT, 'Package manifest bound'); collected.append(chunk)
                need(count == member.size, 'Truncated tar member')
                rows.append({'path':name,'type':'file','mode':member.mode,'bytes':count,'sha256':checksum.hexdigest()})
                if collected:
                    need(manifest is None, 'Ambiguous root package manifest'); manifest = parse_json(b''.join(collected))
        raw.seek(archive.offset); tail = raw.read(2 * tarfile.RECORDSIZE + 1)
        need(1024 <= len(tail) <= 2 * tarfile.RECORDSIZE and not any(tail), 'Hidden/concatenated package archive payload')
    need(len(roots) == 1 and manifest is not None, 'Single package root/manifest required')
    package_name(manifest.get('name')); need(type(manifest.get('version')) is str, 'Tar package identity required')
    return {'root': next(iter(roots)), 'manifest': manifest, 'members': rows, 'payloadBytes': total}


class BoundedRead:
    def __init__(self, stream):
        self.stream = stream
    def read(self, size=-1):
        need(0 <= size <= 1024**2, 'Unbounded tar metadata read refused')
        return self.stream.read(size)
    def __getattr__(self, name):
        return getattr(self.stream, name)


def validate_catalog(lock, entries, read_content, profile):
    """Body reader is bounded by each size. No npm/cache API or network is invoked."""
    packages, requests = graph(lock, profile)
    need(type(entries) is list and 0 < len(entries) <= MAX_ENTRIES, 'Complete bounded semantic cache catalog required')
    names, tarballs, keys, result = set(), set(), set(), []
    for row in entries:
        need(type(row) is dict and set(row) == {'key','integrity','size','time','originTime','metadata'}, 'Exact semantic entry fields required')
        key = row['key']; need(key not in keys, 'Duplicate cache key'); keys.add(key)
        need(type(row['size']) is int and 0 < row['size'] <= MAX_FILE
             and all(type(row[k]) is int and row[k] > 0 for k in ('time','originTime')), 'Bounded original/seed entry times and bytes required')
        sri(row['integrity']); body = read_content(row['integrity'], row['size'])
        need(type(body) is bytes and len(body) == row['size'] and body_sri(body) == row['integrity'], 'Cache body/SRI differs')
        kind, identity = classify_key(key, packages, requests)
        detail = {'key': key, 'body': pin(body), 'integrity': row['integrity'], 'role': kind}
        if key.startswith('make-fetch-happen:request-cache:'):
            _metadata(row['metadata'], key[len('make-fetch-happen:request-cache:'):])
        else:
            need(row['metadata'] is None, 'Unexpected pacote metadata refused')
        if kind == 'metadata':
            value = parse_json(body, MAX_FILE); need(value.get('name') == identity and type(value.get('versions')) is dict, 'Original packument required')
            for selected in packages.values():
                if selected['name'] != identity:
                    continue
                version = value['versions'].get(selected['version']); need(type(version) is dict and version.get('name') == identity and version.get('version') == selected['version'], 'Selected metadata version absent')
                dist = version.get('dist', {}); need(dist.get('tarball') == selected['resolved'] and dist.get('integrity') == selected['integrity'], 'Metadata selected URL/SRI differs')
            names.add(identity)
        else:
            candidates = [p for p in packages.values() if p['name'] not in LOCAL_NAMES and p['integrity'] == row['integrity']
                          and (p['resolved'] == identity if kind == 'tarball' else p['name'] == identity)]
            need(candidates, 'Tarball outside recorded registry graph')
            parsed = package_tar(body)
            for selected in candidates:
                need(parsed['manifest']['name'] == selected['name'] and parsed['manifest']['version'] == selected['version'], 'Tarball package identity differs')
                for field in ('dependencies','optionalDependencies','peerDependencies'):
                    need(mappings(parsed['manifest'], field) == mappings(selected, field), 'Tar/lock dependency graph differs')
                tarballs.add((selected['name'],selected['version'],selected['integrity']))
            detail['package'] = parsed
        result.append(detail)
    selected = [row for row in packages.values() if row['name'] not in LOCAL_NAMES]
    need({row['name'] for row in selected} <= names <= set(requests) - set(LOCAL_NAMES)
         and tarballs == {(r['name'],r['version'],r['integrity'])for r in selected}, 'Incomplete metadata/transitive tarball closure')
    return result


def cache_inventory(root):
    root = Path(root); need(root.is_dir() and not root.is_symlink(), 'Ordinary owned cache root required')
    rows, total = [], 0
    def visit(path):
        nonlocal total
        name = safe_path(path.relative_to(root).as_posix()); info = path.lstat(); mode = stat.S_IMODE(info.st_mode)
        need(not mode & 0o022 and not mode & 0o7000, 'Unsafe cache mode')
        if stat.S_ISDIR(info.st_mode):
            rows.append({'path':name,'type':'directory','mode':mode})
            for child in sorted(path.iterdir()):
                visit(child)
        else:
            need(stat.S_ISREG(info.st_mode) and info.st_nlink == 1 and info.st_size <= MAX_FILE, 'Ordinary single-link cache file required')
            total += info.st_size; need(total <= MAX_PAYLOAD, 'Cache payload bound')
            rows.append({'path':name,'type':'file','mode':mode,**pin(path.read_bytes())})
        need(len(rows) <= MAX_ENTRIES, 'Cache entry bound')
    for path in sorted(root.iterdir()):
        visit(path)
    return rows


def validate_cache_tree(root, entries, lock, profile):
    root = Path(root); rows = cache_inventory(root); by = {r['path']: r for r in rows}; files = set(); directories = {'_cacache','_cacache/tmp'}
    for row in entries:
        name = index_path(row['key']); files.add(name); files.add(content_path(row['integrity']))
        b = (root / name).read_bytes(); need(len(b) <= 65536 and b.startswith(b'\n') and b.count(b'\n') == 1, 'Fresh single-record cache index required')
        checksum, text = b[1:].split(b'\t', 1); need(checksum.decode() == hashlib.sha1(text).hexdigest(), 'Cache index checksum differs')
        actual = parse_json(text); need(set(actual) in ({'key','integrity','time','size'},{'key','integrity','time','size','metadata'}), 'Unexpected cache index fields')
        need(all(actual.get(k) == row[k] for k in ('key','integrity','time','size','metadata')), 'Actual index and semantic catalog differ')
    for path in files:
        directories.update(str(p) for p in PurePosixPath(path).parents if str(p) != '.')
    # A missing empty tmp directory is harmless; an arbitrary empty directory is not.
    expected_dirs = directories & set(by)
    need(set(by) == files | expected_dirs and '_cacache' in by and all(by[n]['type'] == 'directory' for n in expected_dirs)
         and all(by.get(n, {}).get('type') == 'file' for n in files), 'Unreferenced/nonregistry cache payload refused')
    def read(integrity, size):
        path = root / content_path(integrity); need(path.stat().st_size == size, 'Content reference length differs'); return path.read_bytes()
    return {'inventory': rows, 'entries': validate_catalog(lock, entries, read, profile)}


def fresh_target(parent, target):
    parent, target = Path(parent), Path(target)
    need(parent.is_absolute() and target.is_absolute() and parent.is_dir() and not parent.is_symlink(), 'Owned absolute ordinary parent required')
    need(target.parent == parent and not target.exists() and not target.is_symlink(), 'Fresh direct cache target required')
    return True  # Filesystem preflight only; no artifact, execution or host authority.


def validate_terminal(row, seconds=60):
    need(type(row) is dict and row.get('waitReaped') is True and row.get('exitCode') == 0
         and row.get('timedOut') is False and row.get('streamsClosed') is True and row.get('descendantsClosed') is True,
         'Original command terminal/closure required')
    need(type(row.get('elapsedSeconds')) in (int,float) and 0 <= row['elapsedSeconds'] < seconds, 'Original command cap exceeded')
    need(type(row.get('closureObservations')) is list and len(row['closureObservations']) == 2
         and all(type(value) is dict and value.get('live') == [] for value in row['closureObservations'])
         and row.get('readerErrors') == [], 'Two bounded closure observations without reader failure required')
    for name in ('stdout','stderr'):
        value = row[name]; body = base64.b64decode(value['base64'], validate=True)
        need(len(body) <= MAX_STREAM and pin(body) == {k:value[k]for k in ('bytes','sha256')} and value.get('eof') is True, 'Original bounded stream bytes/EOF differ')
    tap = base64.b64decode(row['stdout']['base64']).decode().splitlines()
    need(all(line in tap for line in ['# tests 56','# pass 56','# fail 0','# cancelled 0','# skipped 0']), 'Unchanged original56 packed controls required')


def command_plan(layout, profile):
    """Exact v2 original commands. Paths describe custody; this function grants no execution."""
    need(type(layout) is dict and set(layout) == {'productRoot','workRoot','toolsRoot','node','npm'}, 'Complete command layout required')
    for path in layout.values():
        need(type(path) is str and path.startswith('/') and len(path) <= 4096 and safe_path(path[1:]), 'Canonical absolute command path required')
    repo,work,tools,node,npm = (layout[k] for k in ('productRoot','workRoot','toolsRoot','node','npm'))
    need(node.endswith('/bin/node') and npm == str(PurePosixPath(node).parent.parent/'lib/node_modules/npm/bin/npm-cli.js'), 'Original Node/npm layout required')
    plan=[]
    def add(label,argv,cwd,seconds):plan.append({'label':label,'argv':argv,'cwd':cwd,'limitSeconds':seconds})
    for label,argv in [('node-version',[node,'--version']),('npm-version',[node,npm,'--version']),('bun-version',['bun','--version']),('typescript-version',['bun','-p','process.versions.typescript'])]:
        add(label,argv,repo,15)
    def prepare(path,label):
        add(label+'-npm-root',[node,npm,'ci','--allow-git=all','--ignore-scripts=false'],path,1200)
        add(label+'-npm-native',[node,npm,'ci','--allow-git=all','--ignore-scripts=false','--prefix','packages/backend-mongodb'],path,1200)
        add(label+'-whole-check',['bun','check','-p','tsconfig.json'],path,60)
        add(label+'-adapter-check',['bun','check','-p','packages/backend-mongodb/tsconfig.json'],path,60)
        add(label+'-native-build',[node,npm,'run','build','--prefix','packages/backend-mongodb'],path,120)
        add(label+'-root-build',[node,path+'/node_modules/typescript/bin/tsc','-p','tsconfig.json'],path,120)
    prepare(repo,'resolver-source')
    packs=work+'/resolver/packs';bridge=tools+'/scripts/hapi-packed-registry/cache-bridge.mjs'
    add('resolver-pack-adapter',[node,npm,'pack','--ignore-scripts','--pack-destination',packs],repo+'/packages/backend-mongodb',60)
    add('resolver-pack-sdk',[node,npm,'pack','--ignore-scripts','--pack-destination',packs],repo+'/packages/backend-mongodb/node_modules/@eosrio/hyperion-sdk',60)
    tarballs=[packs+'/'+name.removeprefix('@').replace('/','-')+'-0.0.0-dev.1.tgz' for name in LOCAL_NAMES]
    add('normal-resolver',[node,npm,'install','--ignore-scripts','--no-audit','--no-fund',*sorted(tarballs),'@types/node@25.5.0'],work+'/resolver/consumer',60)
    add('export-registry',[node,bridge,'export',work+'/resolver/export.json'],tools,120)
    proof=work+'/proof-product'
    add('fresh-proof-clone',['git','clone','--no-local','--no-hardlinks','--no-checkout',repo,proof],work,120)
    add('fresh-proof-checkout',['git','checkout','--detach',profile['product']['productHead']],proof,60)
    prepare(proof,'proof-source')
    add('unchanged-offline-packed',[node,npm,'run','test:package','--prefix','packages/backend-mongodb'],proof,60)
    add('actual-pack-output-custody',[node,bridge,'outputs',work+'/output-spec.json'],tools,60)
    return plan


def validate_command_terminal(row, planned):
    need(type(row) is dict and all(row.get(k) == planned[k] for k in ('label','argv','cwd','limitSeconds')), 'Exact original command identity/order/cap required')
    need(row.get('waitReaped') is True and type(row.get('exitCode')) is int and row['exitCode'] == 0
         and row.get('timedOut') is False and row.get('streamsClosed') is True and row.get('descendantsClosed') is True
         and row.get('readerErrors') == [] and row.get('diagnosticErrors') == [], 'Original successful command outcome required')
    finite=lambda v:type(v) in (int,float) and math.isfinite(v)
    start,elapsed=row['startedMonotonic'],row['elapsedSeconds']
    need(finite(start) and start >= 0 and finite(elapsed) and 0 <= elapsed < planned['limitSeconds'], 'Original command timing/cap required')
    end=start+elapsed;direct=row['directExit'];eof=row['pipeEofTimes']
    need(direct['waitReaped'] is True and type(direct['exitCode']) is int and direct['exitCode'] == row['exitCode']
         and finite(direct['at']) and start <= direct['at'] <= end, 'Original direct-exit agreement required')
    need(set(eof) == {'stdout','stderr'} and all(finite(at) and start <= at <= end for at in eof.values()), 'Original stream EOF timestamps required')
    for name in ('stdout','stderr'):
        value=row[name];body=base64.b64decode(value['base64'],validate=True)
        need(value.get('eof') is True and len(body) <= MAX_STREAM and pin(body) == {k:value[k]for k in ('bytes','sha256')}, 'Original complete stream bytes/hash/EOF required')
    closures=row['closureObservations']
    need(type(closures) is list and len(closures) == 2 and all(item['complete'] is True and item['live'] == []
         and finite(item['at']) and start <= item['at'] <= end for item in closures)
         and max(direct['at'],*eof.values()) <= closures[0]['at'] < closures[1]['at'], 'Original post-exit/EOF closure observations required')


def no_authority(value):
    need(type(value) is dict and not ({'executionReady','hostAdmission','continuation','exampleOnly'} & set(value)), 'Evidence does not grant operational readiness')
    return value


def validate_profile(profile, npm_members):
    no_authority(profile)
    pidns = profile.get('id') == 'current103-packed-registry-pidns-v2'
    need(profile.get('schemaVersion') == (2 if pidns else 1)
         and profile.get('id') == ('current103-packed-registry-pidns-v2' if pidns else 'current103-packed-registry-v1')
         and profile.get('kind') == ('hyperion-current103-packed-registry-pidns-v2' if pidns else 'hyperion-current103-packed-registry-v1'), 'Named fixed supplement required')
    product = profile['product']
    need(product['productHead'] == 'bca518ddfebf04c2af957832f2ad9c0582c17932'
         and product['productTree'] == '26cfd37e658d030090059096afc32a574ed787ef'
         and product['sourceFiles'] == 1066
         and product['sourceInventorySha256'] == 'e5334bae500bc1afd70f497f69791fc1d0d8b86dc4aca8c482aaef2373359ba3', 'Fixed BCA source required')
    need(product['dependencySources']['rootSdk']['packagePath'] == 'node_modules/@eosrio/hyperion-sdk'
         and product['dependencySources']['rootSdk']['resolved'].endswith('#7412889a14c84342551fd864ea3ebfcf28849385')
         and product['dependencySources']['adapterSdk']['packagePath'] == 'packages/backend-mongodb/node_modules/@eosrio/hyperion-sdk'
         and product['dependencySources']['adapterSdk']['resolved'].endswith('#7412889a14c84342551fd864ea3ebfcf28849385')
         and product['dependencySources']['wire']['resolved'].endswith('#69a66064c077a405341f5aaf15e84b88b8d86ccb'), 'Current103 source dependency pins required')
    need(profile['consumer'] == {'path':'packages/backend-mongodb/scripts/check-package.mjs','bytes':3335,
         'sha256':'9ce817fb35b613ed709949ba4c68f6e748f45c28e55b6ad1992d644e571471f7','nativeTests':56}, 'Unchanged consumer source required')
    need(profile['registry'] == REGISTRY and profile['localPackages'] == list(LOCAL_NAMES)
         and profile['registryRoots'] == {'@types/node':'25.5.0','mongodb':'7.2.0','ajv':'8.20.0','semver':'7.8.5'}, 'Closed consumer registry roots required')
    need(profile['retainedReceiverLimits'] == {'componentSeconds':300,'packedCommandSeconds':60,'cpus':6,
         'memoryBytes':17179869184,'swapBytes':0,'reserveBytes':48318382080,'diskGiB':96}, 'Original receiver limits must remain unchanged')
    need(profile['offlineEnvironment'] == {'npm_config_offline':'true','npm_config_update_notifier':'false'}, 'Named strict offline environment required')
    need(profile['sourceEnvironment'] == {'HYPERION_NATIVE_SOURCE_PROFILE':'stateful-owner-v2-7412889-69a6606-complete-kind-engineering'}, 'Original engineering source profile required')
    need(profile['cacheFormats'] == {'index':5,'content':2,'integrity':'sha512'}, 'Exact npm cache format required')
    stem = 'current103-packed-registry-pidns-v2' if pidns else 'current103-packed-registry-v1'
    pins = {'zip':None,'receipt':None,'archive':None,'outputs':None}
    artifacts = {'name':'hapi-'+stem+'-linux-x64-node26.4.0','archive':stem+'.tar.gz',
                 'receipt':stem+'.receipt.json','outputs':stem+'.OUTPUT.tar.gz'}
    if pidns:
        pins['context'] = None; artifacts['context'] = stem+'.context.json'
        context = profile['executionContext']
        need(set(context) == {'kind','sourceFiles','maxProcesses','lastCensusIntervalSeconds'}
             and context['kind'] == 'current103-packed-registry-private-pidns-v2'
             and context['maxProcesses'] == 4096 and context['lastCensusIntervalSeconds'] == 5, 'Explicit bounded private PID namespace required')
        need(set(context['sourceFiles']) == {'namespace_contracts.py','namespace_init.py','namespace_launch.py','pidns_runner.py','producer.py','contracts.py','cache-bridge.mjs'}
             and all(set(pin) == {'bytes','sha256'} and type(pin['bytes']) is int and 0 < pin['bytes'] <= 256*1024
                     and re.fullmatch('[a-f0-9]{64}',pin['sha256']) for pin in context['sourceFiles'].values()), 'Complete pinned namespace source files required')
    need(profile['artifactPins'] == pins, 'Examples/source profiles cannot admit artifacts')
    need(profile['workflow'] == ('.github/workflows/hapi-current103-packed-registry-pidns.yml' if pidns else '.github/workflows/hapi-current103-packed-registry.yml')
         and profile['job'] == ('registry-supplement-pidns' if pidns else 'registry-supplement')
         and profile['artifact'] == artifacts, 'Explicit fixed producer/artifact identities required')
    runtime = profile['runtime']
    need(runtime['npmMembers'] == {'path':'npm-members.json','bytes':510548,'sha256':'27ab9f2b7842343504b8d7cbae8be1b8f985f9e4d6237dd6318aee543e55ac57','entries':2376}, 'Exact derived npm member body required')
    need(runtime['os'] == 'linux' and runtime['architecture'] == 'x64' and runtime['node'] == 'v26.4.0'
         and runtime['nodeSha256'] == '4cfdaeec2e3689e4728b4bc98932a9147a3f98162bdc7955c03c0d7fa3b8aa94'
         and runtime['npm'] == '11.17.0' and runtime['bun'] == '1.4.4' and runtime['typescript'] == '7.0.2', 'Actual fixed runtime required')
    need(npm_members['originFullNodeManifest'] == {'bytes':1259431,'sha256':'98fefd07bd7b5faf4eceae81127da0d22a610e2c0b225ff085446b6a8666d7bd'}
         and npm_members['prefix'] == 'lib/node_modules/npm' and len(npm_members['entries']) == 2376, 'Complete original npm runtime baseline required')
    need(runtime['npmPackage'] == {'bytes':6597,'sha256':'a4f04ad50219f82e0e8f262d3e1acbdbd678040198ea36fca10fe7bbb38da726'}
         and runtime['npmCli'] == {'bytes':54,'sha256':'8e5f6f3429f8cdbe693cdc29904e9d5a7b127a494bd15c804bd54c7403bfcbe7'}, 'Full npm identity differs from actual guest')
    return profile


def load_contract(profile_bytes, npm_bytes):
    profile, npm_members = parse_json(profile_bytes), parse_json(npm_bytes)
    validate_profile(profile, npm_members)
    need(pin(npm_bytes) == {k:profile['runtime']['npmMembers'][k]for k in ('bytes','sha256')}, 'Raw npm member inventory bytes differ; no normalization')
    return profile, npm_members


def verify_runtime_members(actual, expected):
    """Exact relative names/types/modes/content, never a semantic-version-only comparison."""
    need(type(actual) is list and len(actual) == len(expected) == 2376, 'All2376 npm members required')
    def index(rows):
        result = {}
        for row in rows:
            need(type(row) is dict, 'Typed npm member required')
            name = safe_path(row['path']); need(name.startswith('lib/node_modules/npm/') or name == 'lib/node_modules/npm', 'Npm member prefix differs')
            need(name not in result and row['type'] in ('dir','file'), 'Duplicate/link npm member refused')
            need(set(row) == ({'path','mode','type'} if row['type'] == 'dir' else {'path','mode','type','bytes','sha256'}), 'Exact npm member fields required')
            result[name] = row
        return result
    need(index(actual) == index(expected), 'Actual npm member set/bytes/modes differs')


def verify_archive(path, rows):
    """Read-only exact tar/tree equality; extraction is deliberately not a receiver here."""
    need(Path(path).stat().st_size <= MAX_ARCHIVE and 0 < len(rows) <= MAX_ENTRIES, 'Cache archive bound')
    by = {r['path']:r for r in rows}; need(len(by) == len(rows), 'Duplicate inventory member')
    seen, total = set(), 0
    with gzip.open(path, 'rb') as raw, tarfile.open(fileobj=BoundedRead(raw), mode='r:') as archive:
        for member in archive:
            name = member.name[:-1] if member.isdir() and member.name.endswith('/') else member.name
            safe_path(name); need(name not in seen and name in by, 'Duplicate/extra archive payload'); seen.add(name)
            row = by[name]; need(not member.sparse and set(member.pax_headers) <= {'path'}, 'Unsupported archive metadata')
            need(member.mode == row['mode'] and not member.mode & 0o7022, 'Cache archive mode differs')
            if row['type'] == 'directory':
                need(member.isdir() and member.size == 0, 'Directory type drift')
            else:
                need(row['type'] == 'file' and member.type in (tarfile.REGTYPE,tarfile.AREGTYPE)
                     and member.size == row['bytes'] and 0 <= member.size <= MAX_FILE, 'Cache file type/size drift')
                total += member.size; need(total <= MAX_PAYLOAD, 'Expanded cache archive bound')
                digest, count = hashlib.sha256(), 0
                with archive.extractfile(member) as stream:
                    for body in iter(lambda:stream.read(1024**2), b''):
                        digest.update(body); count += len(body)
                need(count == row['bytes'] and digest.hexdigest() == row['sha256'], 'Cache tar content differs')
        raw.seek(archive.offset); tail = raw.read(2 * tarfile.RECORDSIZE + 1)
        need(1024 <= len(tail) <= 2 * tarfile.RECORDSIZE and not any(tail), 'Hidden/concatenated cache archive payload')
    need(seen == set(by), 'Complete cache archive inventory required')


def validate_receipt(receipt, profile, profile_pin):
    no_authority(receipt)
    need(receipt.get('schemaVersion') == profile['schemaVersion'] and receipt.get('kind') == profile['kind'] and receipt.get('profile') == profile_pin, 'Source-selected supplement receipt required')
    need(receipt['product'] == profile['product'] and receipt['consumer'] == profile['consumer'], 'Exact source/consumer receipt required')
    producer = receipt['producer']
    need(producer['repository'] == 'eosrio/hyperion-tools' and producer['workflowPath'] == profile['workflow'] and producer['job'] == profile['job']
         and re.fullmatch('[a-f0-9]{40}', producer['workflowRevision']) and re.fullmatch('[a-f0-9]{40}', producer['workflowTree'])
         and all(type(producer[k]) is int and producer[k] > 0 for k in ('runId','runAttempt')), 'Actual CI workflow/run identity required')
    need(receipt['runtime'] == profile['runtime'] and receipt['offlineEnvironment'] == profile['offlineEnvironment'], 'Exact actual runtime and closed offline environment required')
    need(receipt['scope'] == {'isolatedPackedConsumer':True,'fullWireComponent':'NOT_RUN','receiverLimitsUnchanged':profile['retainedReceiverLimits']}, 'Isolated consumer is not full Wire acceptance')
    need(receipt['seedSealedBeforeProof'] is True and receipt['seedContainsLocalPacksOrBuilds'] is False and receipt['freshProofSourceAndBuild'] is True,
         'Fresh source/registry-only sealed seed boundary required')
    validate_terminal(receipt['proof'])
    need(type(receipt['actualOutputPacks']) is list and len(receipt['actualOutputPacks']) == 2
         and {p['name']for p in receipt['actualOutputPacks']} == set(LOCAL_NAMES)
         and all(p['role'] == 'OUTPUT' and p['origin'] == 'actual-proof-mutable-cache-after-original-terminal' for p in receipt['actualOutputPacks']), 'Actual selected output custody required; never a seed')
    need(receipt.get('postUploadArtifact') is None, 'API artifact identity remains separate until Root review')
    if profile['schemaVersion'] == 2:
        import namespace_contracts as N
        value = receipt['executionContext']
        need(value['kind'] == N.KIND and value['contextPin'] == value['before']['contextPin'] == value['after']['contextPin'], 'Original namespace context pin required')
        need(value['start']['kind'] == N.KIND and value['start']['profile'] == profile_pin and value['start']['initDumpable'] == 1
             and value['before']['actual']['pid'] == value['after']['actual']['pid'], 'Original producer context/source identity required')
        for stage in ('before','after'):
            N.validate_snapshot(value[stage]['actual'],value['start']['host'],value['start']['uid'],value['start']['gid'])
            need(value[stage]['actual']['init']['birth'] == value['start']['init']['init']['birth']
                 and value[stage]['actual']['pidNamespace'] == value['start']['init']['pidNamespace'], 'Producer init/namespace lifetime differs')
        plan=command_plan(receipt['commandLayout'],profile)
        need(type(receipt['commands']) is list and len(receipt['commands']) == len(plan), 'Complete nonempty original command sequence required')
        for command,planned in zip(receipt['commands'],plan):
            validate_command_terminal(command,planned)
            need(command['closurePolicy'] == N.KIND and command['diagnosticErrors'] == []
                 and command['namespaceBefore']['contextPin'] == command['namespaceAfter']['contextPin'] == value['contextPin']
                 and command['lastCompleteCensus']['complete'] is True and command['lastCompleteCensus']['live'] == []
                 and len(command['closureObservations']) == 2 and all(row['complete'] is True and row['live'] == [] for row in command['closureObservations']),
                 'Every v2 command needs complete original namespace closure evidence')
        packed=receipt['commands'][-2]
        need(packed['label'] == 'unchanged-offline-packed' and receipt['proof'] == packed, 'Original packed proof must equal its actual command row')
        need(all(later['startedMonotonic'] >= earlier['startedMonotonic']+earlier['elapsedSeconds']
                 for earlier,later in zip(receipt['commands'],receipt['commands'][1:])), 'Original sequential command timing required')
        for command,key in zip(receipt['commands'][:4],('node','npm','bun','typescript')):
            need(base64.b64decode(command['stdout']['base64']).decode().strip() == profile['runtime'][key], 'Original runtime version output differs')
    return receipt
