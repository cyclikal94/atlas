# Running Atlas

Build with `cargo build --locked -p atlas-server`. By default the server uses
`sqlite://atlas.sqlite?mode=rwc` and listens on `127.0.0.1:3000`. Set
`ATLAS_DATABASE_URL` and `ATLAS_BIND` before bootstrap and serving. PostgreSQL URLs
must point to a dedicated writable database; Atlas does not route to read replicas.

Create a local account with `target/debug/atlas-server account alice`; enter a
12–1024-byte password on stdin and close stdin. The command prints the account ID.
Use `invite` for a one-use signup token, or `password USERNAME` for operator password
recovery. Start `target/debug/atlas-server` with no arguments to serve requests.

## Database lifecycle

Atlas is unreleased. Current startup creates the complete schema transactionally,
or validates its baseline identity. Incompatible experimental databases produce a
reset-required error. There is no automatic drop, upgrade adapter or data conversion.
For a disposable SQLite database, stop all users of the file and choose a new file.
For PostgreSQL, provision a fresh dedicated database explicitly. Retain any data you
need before a reset; do not point reset/testing workflows at personal data.

SQLite requires local persistent storage and one server process per file. Every
connection enables foreign keys and bounded busy handling; WAL is enabled. PostgreSQL
initialisation uses an advisory transaction lock; SQLite uses an immediate transaction.
The current write gate serialises publication and related state changes on both engines.

Sessions expire independently of content. `ATLAS_SYNC_RETENTION_DAYS` defaults to 90
and accepts 1–3650. Retention removes expired delivery metadata, not tasks/history.
Logs contain request IDs, route templates, status and stable error codes. Sensitive
content, authentication credentials, feed URLs and push tokens must not be logged.
`/health` checks process liveness; `/ready` checks database connectivity. SIGTERM/SIGINT
trigger graceful shutdown.

See [authentication](authentication.md) for browser/OIDC/proxy settings and
[notifications](notifications.md) for integration keys and outbound network settings.
Release backup/restore tooling is not implemented; retain both database and encryption
keys when making operator-managed backups.
