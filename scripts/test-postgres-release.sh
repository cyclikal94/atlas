#!/usr/bin/env bash
# This harness owns its cluster; never accepts an existing database URL.
set -euo pipefail
cd "$(dirname "$0")/.."
atlas_pg_bin="${ATLAS_PG_BIN:-$(pg_config --bindir)}"
atlas_tmp="$(mktemp -d /tmp/atlas-release-pg.XXXXXX)"
atlas_started=false
cleanup() {
  if [ "$atlas_started" = true ]; then "$atlas_pg_bin/pg_ctl" -D "$atlas_tmp/data" -m fast -w stop >/dev/null; fi
  rm -rf "$atlas_tmp"
}
trap cleanup EXIT
"$atlas_pg_bin/initdb" -D "$atlas_tmp/data" --auth-local=trust --auth-host=reject --no-locale -E UTF8 >/dev/null
"$atlas_pg_bin/pg_ctl" -D "$atlas_tmp/data" -l "$atlas_tmp/postgres.log" -o "-k $atlas_tmp -c listen_addresses=''" -w start >/dev/null
atlas_started=true
export PATH="$atlas_pg_bin:$PATH"
export ATLAS_DATABASE_URL="postgresql:///postgres?host=$atlas_tmp&user=$(id -un)"
unset ATLAS_DATABASE_URL_FILE
"${ATLAS_PYTHON:-python3}" scripts/test_replicas.py
"${ATLAS_PYTHON:-python3}" scripts/backup.py backup "$atlas_tmp/export" --engine postgres --offline
"$atlas_pg_bin/createdb" -h "$atlas_tmp" restored
export ATLAS_DATABASE_URL="postgresql:///restored?host=$atlas_tmp&user=$(id -un)"
"${ATLAS_PYTHON:-python3}" scripts/backup.py restore "$atlas_tmp/export" --engine postgres --server "$PWD/target/debug/atlas-server" --offline
if "${ATLAS_PYTHON:-python3}" scripts/backup.py restore "$atlas_tmp/export" --engine postgres --server "$PWD/target/debug/atlas-server" --offline; then
  echo 'Restore incorrectly accepted an occupied database' >&2
  exit 1
fi
atlas_valid="$("$atlas_pg_bin/psql" "$ATLAS_DATABASE_URL" -XAt -v ON_ERROR_STOP=1 -c "SELECT (SELECT count(*) FROM accounts)>=2 AND (SELECT count(*) FROM receipts)>0 AND (SELECT count(*) FROM sessions)=0")"
[ "$atlas_valid" = t ]
