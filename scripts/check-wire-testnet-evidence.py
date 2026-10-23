"""Check exact byte provenance and the bounded archive without extracting files."""
import hashlib
import json
from pathlib import Path, PurePosixPath
import tarfile

root = Path(__file__).resolve().parent.parent
evidence = root / 'docs/qualification/evidence/wire-testnet-20261020'
sha = lambda value: hashlib.sha256(value).hexdigest()
for line in (evidence / 'SHA256SUMS').read_text(encoding='utf-8').splitlines():
    digest, name = line.split('  ', 1)
    path = evidence / name
    assert path.resolve().parent == evidence.resolve(), name
    assert sha(path.read_bytes()) == digest, name
archive = evidence / 'wire-testnet-h3-preflight-20261020.tar.gz'
assert sha(archive.read_bytes()) == 'c3faa29f3980ab7309e5648177c5637dcda58543a3f7086f4eec9cffa409fcbb'
manifest_bytes = (evidence / 'evidence-manifest.json').read_bytes()
assert sha(manifest_bytes) == '3aea9f03ab06421f7bddfc678be17f9872c1f9f4ae6b1149cee1786fe7674c74'
manifest = json.loads(manifest_bytes)
expected = {row['path']: row for row in manifest['members']}
assert len(expected) == len(manifest['members'])
with tarfile.open(archive, mode='r:gz') as bundle:
    members = bundle.getmembers()
    assert len(members) == 44
    names = [row.name for row in members]
    assert len(names) == len(set(names))
    assert set(names) == set(expected) | {'evidence-manifest.json'}
    assert sum(name.startswith('raw/') for name in names) == 32
    for member in members:
        path = PurePosixPath(member.name)
        assert member.isfile() and not path.is_absolute() and '..' not in path.parts
        assert ':' not in member.name and '\\' not in member.name
        # A fixed small archive and explicit per-member bounds avoid unbounded reads.
        if member.name == 'evidence-manifest.json':
            assert member.size == len(manifest_bytes)
            assert bundle.extractfile(member).read() == manifest_bytes
        else:
            entry = expected[member.name]
            assert member.size == entry['bytes'] <= 150_000
            data = bundle.extractfile(member).read()
            assert len(data) == entry['bytes'] and sha(data) == entry['sha256'], member.name
            if member.name == 'ship-preflight-20261020.json':
                assert data == (evidence / member.name).read_bytes()
for major in (22, 24):
    report = json.loads((evidence / f'installed-codec-node{major}.json').read_bytes())
    assert report['passed'] and report['runtime']['version'].startswith(f'v{major}.')
    assert report['inputSha256'] == expected['ship-preflight-20261020.json']['sha256']
    assert report['networkAttempts'] == 0 and report['componentFiles'] == 68
    assert not report['fullInitialTableCompletenessValidated']
    assert not report['canonicalAbiBootstrapQualified'] and not report['nativeWorkerAcceptance']
print('Wire actual-testnet evidence: 44 archived members, 32 original byte files and both Node reports verified')
