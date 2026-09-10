"""Capture immutable experiment sources, or verify an existing hash manifest.

capture OUTPUT must run before building an experiment, into a new directory.
verify OUTPUT uses OUTPUT/source; --source-root checks an explicitly chosen tree.
Historical Linux evidence has hashes only: verifying it against today's tree is
expected to fail after source changes. Never rewrite those historical hashes.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess

root = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('command', choices=['capture', 'verify'])
parser.add_argument('output', type=Path)
parser.add_argument('--source-root', type=Path)
args = parser.parse_args()
manifest = args.output / 'source-sha256.json'
if args.command == 'capture':
    if args.source_root:
        parser.error('capture always uses the repository sources')
    args.output.mkdir(parents=True, exist_ok=False)
    paths = [root / name for name in ['Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml']]
    for directory in ['spikes', 'crates', 'scripts', '.github']:
        paths.extend(p for p in (root / directory).rglob('*') if p.is_file()
                     and '__pycache__' not in p.parts)
    hashes = {}
    for path in sorted(paths):
        relative = path.relative_to(root)
        data = path.read_bytes()
        target = args.output / 'source' / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(data)
        hashes[str(relative)] = hashlib.sha256(data).hexdigest()
    manifest.write_text(json.dumps(hashes, indent=2) + '\n')
    revision = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=root, text=True).strip()
    dirty = bool(subprocess.check_output(['git', 'status', '--porcelain'], cwd=root))
    (args.output / 'provenance.json').write_text(json.dumps({
        'base_revision': revision, 'working_tree_dirty': dirty,
        'source_snapshot': 'source', 'note': 'Hashes identify captured inputs, not a performance result.'
    }, indent=2) + '\n')
source = args.source_root or args.output / 'source'
failures = []
for name, expected in json.loads(manifest.read_text()).items():
    path = source / name
    if not path.is_file() or hashlib.sha256(path.read_bytes()).hexdigest() != expected:
        failures.append(name)
if failures:
    raise SystemExit('Source mismatch: ' + ', '.join(failures))
print('Verified source manifest:', manifest)
