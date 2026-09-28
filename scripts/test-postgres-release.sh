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
atlas_py="${ATLAS_PYTHON:-python3}"
# Recovery checks that need SQL and real servers live in one helper (documented in its header).
export ATLAS_PG_CLUSTER="$atlas_tmp"
atlas_source_url="$ATLAS_DATABASE_URL"
atlas_restored_url="postgresql:///restored?host=$atlas_tmp&user=$(id -un)"
"$atlas_py" scripts/test_replicas.py
# Row digests before any further server touches the source, so equality after the restore is exact.
"$atlas_py" scripts/test_recovery_postgres.py digest "$atlas_source_url" "$atlas_tmp/source.json"
"$atlas_py" scripts/backup.py backup "$atlas_tmp/export" --engine postgres --offline
"$atlas_pg_bin/createdb" -h "$atlas_tmp" restored
export ATLAS_DATABASE_URL="$atlas_restored_url"
"$atlas_py" scripts/backup.py restore "$atlas_tmp/export" --engine postgres --server "$PWD/target/debug/atlas-server" --offline
"$atlas_py" scripts/test_recovery_postgres.py digest "$atlas_restored_url" "$atlas_tmp/restored.json"
"$atlas_py" scripts/test_recovery_postgres.py compare "$atlas_tmp/source.json" "$atlas_tmp/restored.json"
if "$atlas_py" scripts/backup.py restore "$atlas_tmp/export" --engine postgres --server "$PWD/target/debug/atlas-server" --offline; then
  echo 'Restore incorrectly accepted an occupied database' >&2
  exit 1
fi
# The refused restore left the occupied database exactly as it was.
"$atlas_py" scripts/test_recovery_postgres.py digest "$atlas_restored_url" "$atlas_tmp/restored-after-refusal.json"
"$atlas_py" scripts/test_recovery_postgres.py same "$atlas_tmp/restored.json" "$atlas_tmp/restored-after-refusal.json"
atlas_valid="$("$atlas_pg_bin/psql" "$ATLAS_DATABASE_URL" -XAt -v ON_ERROR_STOP=1 -c "SELECT (SELECT count(*) FROM accounts)>=2 AND (SELECT count(*) FROM receipts)>0 AND (SELECT count(*) FROM operation_outcomes)>0 AND (SELECT count(*) FROM sessions)=0 AND (SELECT count(*) FROM activation_grants)=0")"
[ "$atlas_valid" = t ]
# Real servers on the source and the restored database serve the same history and timers, and the
# restored one accepts a new concurrent timer; then older bundles, a real startup upgrade and refusals.
"$atlas_py" scripts/test_recovery_postgres.py serve "$atlas_source_url" "$atlas_restored_url"
"$atlas_py" scripts/test_recovery_postgres.py legacy "$atlas_tmp/export"
