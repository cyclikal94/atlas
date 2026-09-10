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
Use the [versioned export and restore workflow](recovery.md); retain both database and
encryption keys.

## Docker and Compose

Container deployments use PostgreSQL. The image rejects an absent or SQLite database
URL. SQLite remains supported when running the executable directly on the host, for
local development and local deployments. The pinned image runs as UID/GID 10001 with
a read-only root filesystem, includes runtime libraries and generated licence notices,
and needs no application data volume. PostgreSQL owns persistent storage.

For an existing database, put `ATLAS_DATABASE_URL` in `atlas.env` (mode 0600), then run
`docker compose up -d --build`. `compose.yaml` exposes only loopback port 3000. To
supply PostgreSQL instead, set a long random **URL-safe** `ATLAS_POSTGRES_PASSWORD`
in `.env` (mode 0600), then run:

```sh
docker compose -f compose.yaml -f compose.postgres.yaml up -d --build
```

The supplied database has no host port and uses a persistent named volume. Bootstrap
with `docker compose run --rm -T atlas account alice` (include the PostgreSQL override
when used), entering the password on stdin. Keep the same Compose files for stop,
restart and bootstrap. `down` retains volumes; `down -v` deletes them and is not an
upgrade step. The optional database is a single PostgreSQL instance.

For plain Docker, build `docker build -t atlas-api:local .` and supply a secure env file:

```sh
docker run --name atlas --read-only --cap-drop=ALL --security-opt=no-new-privileges \
  --env-file /secure/atlas.env -p 127.0.0.1:3000:3000 atlas-api:local
```

Set the database URL to a hostname reachable from the container. Mounted credentials
can use the `_FILE` settings in [configuration](configuration.md). Put a TLS reverse
proxy in front for network use and configure `ATLAS_PUBLIC_ORIGIN` and trusted proxy
IPs. `docker stop atlas` sends SIGTERM and permits graceful shutdown.

`python3 scripts/smoke_container.py atlas-api:local` creates its own isolated Docker
network, PostgreSQL volume and containers, exercises persistence and API operations,
then removes those fixtures. CI runs the image on native ARM64 and AMD64 Linux runners.
A workflow definition is not evidence of a remote run until CI actually executes.

## Helm

`deploy/helm/atlas` requires PostgreSQL: an external database or an optional
single-instance StatefulSet. The API uses non-root execution, a read-only root
filesystem, probes and a ClusterIP Service. Configure your ingress/TLS proxy and
`config.ATLAS_PUBLIC_ORIGIN`. Supply a built image repository/tag explicitly:

```sh
helm upgrade --install atlas deploy/helm/atlas \
  --set image.repository=YOUR_REGISTRY/atlas --set image.tag=YOUR_TAG \
  --set database.existingSecret=atlas-database --set replicaCount=2
```

The existing Secret must have `ATLAS_DATABASE_URL`. To supply PostgreSQL, replace
`database.existingSecret` with `postgresql.enabled=true` and
`postgresql.existingSecret=atlas-database`. That Secret contains `POSTGRES_PASSWORD`
and `ATLAS_DATABASE_URL`; the URL points to `atlas-postgres:5432/atlas` for release name
`atlas`, user `atlas`, with a URL-encoded password. Provision Secrets through your
secret manager; do not put passwords in Helm arguments or checked-in values.
`existingSecret` supplies other Atlas credentials. Changing the PostgreSQL Secret does
not change a password in an already-initialised database; rotate it in PostgreSQL too.

The chart supports multiple API replicas. The resource requests/limits are starting
allocations, not measured capacity guarantees. PostgreSQL PVCs remain after StatefulSet
deletion. Stop all API replicas for backup and restore. Rolling application updates
are only safe when their schema and operation contracts are mutually compatible;
see the release policy in [recovery](recovery.md).

## Monitoring and recovery

Monitor `/ready` for database reachability and `/health` for process liveness. Collect
structured stderr logs and correlate failures by request ID and stable error code.
Alert on repeated `maintenance_error`/`integration_error`, restarts, persistent readiness
failures, storage exhaustion and PostgreSQL lock waits. Authenticated calendar-source
and notification-delivery endpoints expose integration health; they are private API
resources, not public metrics. Health responses do not attest provider availability.
No Prometheus endpoint or production throughput guarantee is supplied.

If the database is unavailable, restore connectivity before restarting repeatedly.
If storage is full, free/expand storage without deleting the database or WAL. If workers
stop mid-operation, persisted leases expire and a replica retries; external delivery is
at-least-once. Do not manually clear leases on a running installation. See
[recovery](recovery.md) for restore and rollback, and [configuration](configuration.md)
for the complete environment reference.
