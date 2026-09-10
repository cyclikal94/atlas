#!/usr/bin/env bash
set -euo pipefail
atlas_target="${1:?usage: notices.sh TARGET OUTPUT}"
atlas_output="${2:?usage: notices.sh TARGET OUTPUT}"
cargo about generate --locked --fail --target "$atlas_target" \
  --manifest-path crates/server/Cargo.toml --config about.toml \
  packaging/notices.hbs --output-file "$atlas_output"
# Guard the coverage that originally required hand-maintained notices.
python3 - "$atlas_output" <<'PY'
import pathlib, sys
text = pathlib.Path(sys.argv[1]).read_text()
for required in ['ece ', 'coarsetime ', 'Frank Denis', 'Mozilla Public License']:
    if required not in text:
        raise SystemExit('Generated notices lack expected coverage: ' + required)
PY
