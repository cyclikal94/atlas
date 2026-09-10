#!/usr/bin/env bash
# Run the same domain cases as SQLite, each in an isolated schema.
set -euo pipefail
atlas_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$atlas_root"
if [ -z "${ATLAS_TEST_POSTGRES_URL:-}" ]; then
  if [ -n "${ATLAS_PG_BIN:-}" ]; then
    atlas_pg_bin="$ATLAS_PG_BIN"
  elif command -v pg_config >/dev/null; then
    atlas_pg_bin="$(pg_config --bindir)"
  elif [ -x /opt/homebrew/opt/postgresql@17/bin/initdb ]; then
    atlas_pg_bin=/opt/homebrew/opt/postgresql@17/bin
  else
    echo 'Set ATLAS_PG_BIN to a PostgreSQL bin directory or ATLAS_TEST_POSTGRES_URL to a disposable test database.' >&2
    exit 1
  fi
  atlas_tmp="$(mktemp -d /tmp/atlas-pg.XXXXXX)"
  atlas_started=false
  cleanup() {
    if [ "$atlas_started" = true ]; then
      "$atlas_pg_bin/pg_ctl" -D "$atlas_tmp/data" -m fast -w stop >/dev/null
    fi
    rm -rf "$atlas_tmp"
  }
  trap cleanup EXIT
  "$atlas_pg_bin/initdb" -D "$atlas_tmp/data" --auth-local=trust --auth-host=reject --no-locale -E UTF8 >/dev/null
  "$atlas_pg_bin/pg_ctl" -D "$atlas_tmp/data" -l "$atlas_tmp/server.log" -o "-k $atlas_tmp -c listen_addresses=''" -w start >/dev/null
  atlas_started=true
  export ATLAS_TEST_POSTGRES_URL="postgresql:///postgres?host=$atlas_tmp&user=$(id -un)"
fi
# A domain substring, e.g. tasks::timers, can be passed as Cargo's test filter.
cargo test --locked -p atlas-core --test integration "$@"
cargo test --locked -p atlas-server --test api "$@"
