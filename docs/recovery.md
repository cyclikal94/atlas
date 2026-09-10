# Export, backup and restore

`scripts/backup.py` creates a versioned, same-engine export bundle: a consistent SQLite
backup or PostgreSQL custom dump, a JSON manifest with format/schema/engine and SHA-256
file digests, and optionally an operator-supplied secrets file. This is a full database
export, not a cross-engine transfer or a per-account data export.

Stop **all** Atlas processes and replicas before either operation. `--offline` records
your acknowledgement; the script cannot discover all remote processes. Keep restored
copies isolated until validation is complete. PostgreSQL client tools must support the
server version; Python 3 and a matching Atlas executable are required on the recovery
host. Docker and Kubernetes operators can run these tools from a maintenance host with
access to the database/volume, with application workloads stopped.

```sh
python3 scripts/backup.py backup /secure/backups/atlas-2026-09-10 \
  --engine sqlite --sqlite /data/atlas.sqlite --secrets-file /secure/atlas.env --offline
python3 scripts/backup.py restore /secure/backups/atlas-2026-09-10 \
  --engine sqlite --sqlite /data/restored.sqlite --server target/debug/atlas-server --offline
```

For PostgreSQL, set `ATLAS_DATABASE_URL` or `ATLAS_DATABASE_URL_FILE`, omit `--sqlite`,
and use `--engine postgres`. For restore, point to a **new, empty, dedicated** database.
The script rejects occupied destinations and cross-engine/unsupported-format restores.
It never drops a database. A failed operation can leave an incomplete new bundle or
restored destination: keep Atlas stopped, inspect the failure, and retry with a fresh
path/database. The original database and backup remain intact.

Bundles contain private data, password hashes and potentially tokens. Directories are
created with mode 0700 and files with 0600. Store them on encrypted storage or encrypt
with your backup system before transfer. Hashes detect corruption, not tampering by an
attacker who can replace the manifest. Only restore trusted bundles. The optional
`secrets.env` is copied verbatim and is **not encrypted or automatically applied**.
Preserve `ATLAS_SECRET_KEY`, VAPID keys and OIDC configuration in your secret manager
whether or not you include a secrets file. The script does not discover those secrets.

After loading the data, restore invokes `atlas-server prepare-restore --offline`.
This transaction invalidates login sessions, in-flight OIDC flows, signup invitations,
snapshot cursors and signed delta cursor keys; resets calendar/delivery leases; and
preserves domain data, resource IDs, history and operation receipts. Clients log in
again and replace their cached view with a new snapshot. They must retain pending
operation IDs for retries. Operations committed after the backup point are absent from
the restored copy; this is the backup's recovery point, not a guarantee of zero data loss.
Previously delivered external notifications after that point may be delivered again.

For a database restored using native tools, run `prepare-restore --offline` manually
before starting Atlas. Database preparation is not a migration or a key rotation.
Restore the saved integration keys, start one replica, check `/ready`, log in, verify
content and integration credentials, then enable other PostgreSQL replicas. Keep the
old installation isolated until satisfied. To roll back, stop the new installation and
restore the saved database with its matching executable and keys.

## Release compatibility

Atlas is unreleased. The current backup format is 1 and schema baseline is 1000.
Restore requires that baseline; incompatible experimental schemas are rejected.
At the first published release, this baseline becomes the supported starting point.
Subsequent releases must provide transactional, ordered schema migrations and tests
from every supported prior release before publishing. No down-migration is promised:
rollback restores the pre-upgrade backup and matching binary. Versioned HTTP contracts
and durable operation identities require explicit compatibility decisions at release.
There is no historical released version against which to run an upgrade test yet.
