"""Bind every retained public fixture and generator to its reviewed digest."""
import hashlib
import pathlib
import sys

root = pathlib.Path(__file__).resolve().parent
manifest = root / 'SHA256SUMS'
files = sorted(path for path in root.rglob('*') if path.is_file() and path != manifest
               and '__pycache__' not in path.parts)
current = ''.join(hashlib.sha256(path.read_bytes()).hexdigest() + '  ' +
                  path.relative_to(root).as_posix() + '\n' for path in files)
if sys.argv[1:] == ['--write']:
    manifest.write_text(current, encoding='utf8', newline='\n')
elif sys.argv[1:]:
    raise SystemExit('usage: check-fixtures.py [--write]')
elif manifest.read_text(encoding='utf8') != current:
    raise SystemExit('Fixture content or inventory differs from SHA256SUMS')
else:
    print(f'Verified {len(files)} public codec fixture files')
