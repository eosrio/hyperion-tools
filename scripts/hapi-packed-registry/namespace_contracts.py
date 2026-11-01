"""Pure PID-namespace data contracts; Linux reads happen only through explicit functions."""
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import time
import contracts as C

KIND = 'current103-packed-registry-private-pidns-v2'
MAX_PROCESSES = 4096
CI_KEYS = ('GITHUB_ACTIONS','GITHUB_REPOSITORY','GITHUB_JOB','GITHUB_WORKFLOW_REF',
           'GITHUB_WORKFLOW_SHA','GITHUB_RUN_ID','GITHUB_RUN_ATTEMPT','RUNNER_TRACKING_ID','PATH')


def selected_environment(original, profile):
    result = {name: original[name] for name in CI_KEYS}
    C.need(result['GITHUB_ACTIONS'] == 'true' and result['GITHUB_REPOSITORY'] == 'eosrio/hyperion-tools'
           and result['GITHUB_JOB'] == profile['job']
           and result['GITHUB_WORKFLOW_REF'].startswith('eosrio/hyperion-tools/' + profile['workflow'] + '@'), 'Actual selected CI identity required')
    C.need(re.fullmatch('[a-f0-9]{40}',result['GITHUB_WORKFLOW_SHA'])
           and all(result[k].isdigit() and int(result[k]) > 0 for k in ('GITHUB_RUN_ID','GITHUB_RUN_ATTEMPT')), 'Actual CI revision/run required')
    C.need(0 < len(result['PATH']) <= 32768 and all(part.startswith('/') for part in result['PATH'].split(':')), 'Absolute bounded original CI PATH required')
    C.need(type(result['RUNNER_TRACKING_ID']) is str and 0 < len(result['RUNNER_TRACKING_ID']) <= 256, 'Original CI lifecycle tracking required')
    return result


def source_pins(profile, directory):
    expected = profile['executionContext']['sourceFiles']
    C.need(set(expected) == {'namespace_contracts.py','namespace_init.py','namespace_launch.py','pidns_runner.py','producer.py','contracts.py','cache-bridge.mjs'}, 'Complete namespace executable closure required')
    for name,pin in expected.items():
        path = Path(directory)/name; info = path.lstat()
        C.need(stat.S_ISREG(info.st_mode) and info.st_nlink == 1 and info.st_size <= 256*1024
               and C.pin(path.read_bytes()) == pin, 'Namespace source pin differs: ' + name)


def parse_stat(pid, body):
    C.need(type(pid) is int and pid > 0 and type(body) is str and len(body) <= 4096, 'Bounded process stat required')
    opening,closing = body.find('('),body.rfind(')')
    C.need(opening > 0 and closing > opening and body[:opening].strip() == str(pid), 'Process stat identity differs')
    fields = body[closing+2:].split(); C.need(len(fields) >= 20 and len(fields[0]) == 1, 'Complete process stat required')
    return {'pid':pid,'comm':body[opening+1:closing],'state':fields[0],'ppid':int(fields[1]),
            'group':int(fields[2]),'session':int(fields[3]),'birth':int(fields[19])}


def read_text(path, maximum=65536):
    with Path(path).open('r') as stream:
        body = stream.read(maximum+1)
    C.need(len(body) <= maximum, 'Bounded proc/source text required'); return body


def snapshot():
    status = dict(line.split(':',1) for line in read_text('/proc/self/status').splitlines() if ':' in line)
    return {'pid':os.getpid(),'ppid':os.getppid(),'uid':os.getuid(),'gid':os.getgid(),
            'realUids':list(os.getresuid()),'realGids':list(os.getresgid()),
            'statusUids':[int(v) for v in status['Uid'].split()],'statusGids':[int(v) for v in status['Gid'].split()],
            'supplementaryGids':[int(v) for v in status['Groups'].split()],
            'pidNamespace':os.readlink('/proc/self/ns/pid'),'mountNamespace':os.readlink('/proc/self/ns/mnt'),
            'procSelf':os.readlink('/proc/self'),'initNamespace':os.readlink('/proc/1/ns/pid'),
            'init':parse_stat(1,read_text('/proc/1/stat',4096)),
            'procMounted':any(line.split(' - ',1)[-1].split()[:2] == ['proc','proc'] and line.split()[4] == '/proc'
                              for line in read_text('/proc/self/mountinfo').splitlines() if ' - ' in line),
            'capEff':status['CapEff'].strip(),'capPrm':status['CapPrm'].strip(),
            'capInh':status['CapInh'].strip(),'capAmb':status['CapAmb'].strip(),'noNewPrivs':status['NoNewPrivs'].strip()}


def host_snapshot():
    return {'pidNamespace':os.readlink('/proc/self/ns/pid'),'mountNamespace':os.readlink('/proc/self/ns/mnt'),
            'pid':os.getpid(),'uid':os.getuid(),'gid':os.getgid()}


def validate_snapshot(value, host, uid, gid, *, init=False):
    C.need(type(uid) is int and uid > 0 and type(gid) is int and gid > 0, 'Original nonroot UID and GID required')
    C.need(value['pidNamespace'] != host['pidNamespace'] and value['mountNamespace'] != host['mountNamespace']
           and re.fullmatch(r'pid:\[\d+\]',value['pidNamespace']) and re.fullmatch(r'mnt:\[\d+\]',value['mountNamespace']), 'Fresh private PID/mount namespaces required')
    C.need(value['procMounted'] is True and value['procSelf'] == str(value['pid'])
           and value['initNamespace'] == value['pidNamespace'] and value['init']['pid'] == 1
           and value['init']['birth'] > 0, 'Namespace-local fresh procfs/init identity required')
    C.need(value['uid'] == uid and value['gid'] == gid and value['realUids'] == [uid]*3
           and value['realGids'] == [gid]*3 and value['statusUids'] == [uid]*4 and value['statusGids'] == [gid]*4
           and value['supplementaryGids'] == [] and value['noNewPrivs'] == '1'
           and all(type(value[key]) is str and re.fullmatch('0+',value[key]) for key in ('capEff','capPrm','capInh','capAmb')),
           'Original unprivileged builder identity and empty capability sets required')
    C.need((value['pid'] == 1 and value['ppid'] == 0) if init else value['pid'] > 1, 'Expected namespace role required')


def current_context(path, profile):
    path = Path(path); info = path.lstat()
    C.need(stat.S_ISREG(info.st_mode) and info.st_nlink == 1 and not stat.S_IMODE(info.st_mode)&0o077
           and info.st_size <= 128*1024, 'Ordinary private init context required')
    raw = path.read_bytes(); value = C.parse_json(raw); C.need(value['kind'] == KIND and value['profile'] == C.pin((path.parent/'profile.json').read_bytes()), 'Exact v2 init context required')
    C.need(C.parse_json((path.parent/'profile.json').read_bytes()) == profile and value['uid'] > 0 and value['gid'] > 0
           and value['initDumpable'] == 1, 'Current complete profile/identity/proc visibility required')
    actual = snapshot(); validate_snapshot(actual,value['host'],value['uid'],value['gid'])
    C.need(actual['pidNamespace'] == value['init']['pidNamespace'] and actual['mountNamespace'] == value['init']['mountNamespace']
           and actual['init']['birth'] == value['init']['init']['birth'], 'Original init/namespace changed')
    return value, {'contextPin':C.pin(raw),'actual':actual}


def census():
    rows = {}
    for path in Path('/proc').iterdir():
        if not path.name.isdigit(): continue
        try:row = parse_stat(int(path.name),read_text(path/'stat',4096))
        except FileNotFoundError:continue
        try:row['pidNamespace'] = os.readlink(path/'ns/pid')
        except FileNotFoundError:
            # A zombie can retain stat while its namespace link has gone. Keep it blocking.
            try:again = parse_stat(int(path.name),read_text(path/'stat',4096))
            except FileNotFoundError:continue
            C.need(again['birth'] == row['birth'], 'PID reused during census')
            row['pidNamespace'] = None;row['namespaceLinkUnavailable'] = True
        try:confirmed = parse_stat(int(path.name),read_text(path/'stat',4096))
        except FileNotFoundError:continue
        C.need(confirmed['birth'] == row['birth'], 'PID reused during census')
        rows[row['pid']] = row
        C.need(len(rows) <= MAX_PROCESSES, 'Complete namespace process-census bound exceeded')
    return rows


def classify(before, rows, observed, session):
    observed = set(observed)
    while True:
        added = {(pid,row['birth']) for pid,row in rows.items()
                 if row['session'] == session or row['ppid'] in rows and (row['ppid'],rows[row['ppid']]['birth']) in observed}
        updated = observed | added
        C.need(len(updated) <= MAX_PROCESSES, 'Bounded command identity history exceeded')
        if updated == observed: break
        observed = updated
    live = []
    for pid,row in rows.items():
        reasons = []
        if (pid,row['birth']) in observed: reasons.append('observed-command-descendant-or-session')
        if before.get(pid,{}).get('birth') != row['birth']: reasons.append('new-identity-in-owned-namespace')
        if reasons: live.append({**row,'reasons':reasons})
    return observed,live


def validate_completion(value, profile, original_exit, receipt_pin, start):
    C.need(value['kind'] == KIND and value['profile'] == profile and value['producerExitCode'] == original_exit
           and original_exit == 0 and value['producerReceipt'] == receipt_pin and value['producerReceiptValidated'] is True,
           'Original successful producer receipt/exit required')
    C.need(value['beforeTeardown'] is True and value['leftoverProcesses'] == [] and value['scanErrors'] == []
           and value['reapCoverageComplete'] is True and type(value['reapCount']) is int and 1 <= value['reapCount'] <= 4096
           and len(value['closureObservations']) == 2 and all(row['live'] == [] for row in value['closureObservations']), 'Complete pre-teardown namespace closure required')
    validate_snapshot(value['initFinal'],start['host'],start['uid'],start['gid'],init=True)
    C.need(value['initFinal']['pidNamespace'] == start['init']['pidNamespace']
           and value['initFinal']['mountNamespace'] == start['init']['mountNamespace']
           and value['initFinal']['init']['birth'] == start['init']['init']['birth'], 'Original init at completion required')
