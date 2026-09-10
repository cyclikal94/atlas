# Server configuration

All replicas use the same database, public origin, integration encryption key and
OIDC/VAPID configuration. Restart after configuration changes. Secrets belong in an
operator-managed secret store; never commit `atlas.env`, `.env` or key files.

| Variable | Default / purpose |
| --- | --- |
| `ATLAS_DATABASE_URL` | `sqlite://atlas.sqlite?mode=rwc`; dedicated writable PostgreSQL URL also supported |
| `ATLAS_BIND` | `127.0.0.1:3000`; image uses `0.0.0.0:3000` |
| `ATLAS_PUBLIC_ORIGIN` | Optional outside browser/OIDC use; exact public origin including scheme |
| `ATLAS_TRUSTED_PROXY_IPS` | Empty; comma-separated exact proxy IPs allowed to supply forwarded information |
| `ATLAS_DIRECTORY_ENABLED` | `true`; server account directory discoverability |
| `ATLAS_SYNC_RETENTION_DAYS` | `90`; integer 1–3650, delivery metadata retention only |
| `ATLAS_SECRET_KEY` | Optional until encrypted integrations are used; 64 hexadecimal characters |
| `ATLAS_OUTBOUND_ALLOW_ORIGINS` | Empty; explicit outbound origin exceptions, see notifications documentation |
| `ATLAS_VAPID_PRIVATE_KEY` | Optional base64url P-256 Web Push key |
| `ATLAS_VAPID_SUBJECT` | Contact URI used with VAPID |
| `ATLAS_OIDC_ISSUER` | Optional OIDC issuer URL; enables provider configuration |
| `ATLAS_OIDC_CLIENT_ID` | Required when configuring an OIDC issuer |
| `ATLAS_OIDC_CLIENT_SECRET` | Optional provider client secret |
| `ATLAS_OIDC_AUTO_PROVISION` | `false`; permit OIDC account creation |
| `ATLAS_OIDC_NATIVE_REDIRECT_URIS` | Empty; comma-separated exact native callback allowlist |

`ATLAS_DATABASE_URL`, `ATLAS_SECRET_KEY`, `ATLAS_VAPID_PRIVATE_KEY` and
`ATLAS_OIDC_CLIENT_SECRET` also accept a `_FILE` suffix to read mounted secret files.
Set the value or the file variable, never both. A trailing newline is removed; an
unreadable or empty file fails startup. Files must be readable by the container UID.
Atlas does not hot-reload or rotate encryption keys. Preserve the existing key through
restore; replacing it makes previously encrypted integration credentials unreadable.

`atlas-server probe` checks `/ready` at loopback port 3000 with a five-second timeout.
It is intended for the supplied container configuration. Custom bind ports should
use HTTP health probes configured with that port.

See [authentication](authentication.md) for TLS/proxy and OIDC behaviour, and
[notifications](notifications.md) for secret formats and outbound restrictions.
