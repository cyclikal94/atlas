#!/usr/bin/env bash
# Package the already-built native executable with its licences and recovery tools.
set -euo pipefail
atlas_target="${1:?usage: package-release.sh TARGET NEW_OUTPUT_DIRECTORY}"
atlas_output="${2:?usage: package-release.sh TARGET NEW_OUTPUT_DIRECTORY}"
atlas_host="$(rustc -vV | sed -n 's/^host: //p')"
if [ "$atlas_target" != "$atlas_host" ]; then
  echo 'Native release packaging requires the compiler host target.' >&2
  exit 1
fi
atlas_dirty=false
if [ -n "$(git status --porcelain)" ]; then atlas_dirty=true; fi
mkdir "$atlas_output"
cp target/release/atlas-server "$atlas_output/"
cp LICENSE Cargo.lock "$atlas_output/"
cp scripts/backup.py "$atlas_output/"
scripts/notices.sh "$atlas_target" "$atlas_output/THIRD_PARTY_NOTICES.txt"
mkdir "$atlas_output/rust"
cp "$(rustc --print sysroot)/share/doc/rust/COPYRIGHT-library.html" "$atlas_output/rust/"
cp -R "$(rustc --print sysroot)/share/doc/rust/licenses" "$atlas_output/rust/"
cp docs/recovery.md docs/configuration.md "$atlas_output/"
if [ "$(uname -s)" = Linux ]; then
  # Dynamic libraries are supplied by the target OS, not bundled into this archive.
  ldd target/release/atlas-server > "$atlas_output/runtime-libraries.txt"
else
  otool -L target/release/atlas-server > "$atlas_output/runtime-libraries.txt"
fi
ATLAS_PACKAGE_DIRTY="$atlas_dirty" ATLAS_PACKAGE_TARGET="$atlas_target" python3 - "$atlas_output" <<'PY'
import hashlib, json, os, pathlib, subprocess, sys
root = pathlib.Path(sys.argv[1])
(root / 'build.json').write_text(json.dumps({
    'revision': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
    'dirty': os.environ['ATLAS_PACKAGE_DIRTY'] == 'true',
    'target': os.environ['ATLAS_PACKAGE_TARGET'],
    'rustc': subprocess.check_output(['rustc', '--version'], text=True).strip(),
    'lock_sha256': hashlib.sha256(pathlib.Path('Cargo.lock').read_bytes()).hexdigest()
}, indent=2) + '\n')
with (root / 'SHA256SUMS').open('w') as out:
    for path in sorted(root.rglob('*')):
        if path.is_file() and path.name != 'SHA256SUMS':
            out.write(hashlib.sha256(path.read_bytes()).hexdigest() + '  ' + str(path.relative_to(root)) + '\n')
PY
