# Reminders and notifications

All routes below use `/api/experimental/v1`.

## Reminders and delivery ownership

`POST /reminder-commands` configures a personal rule on an occurrence. It has a signed
second offset, optional clock time, late-delivery allowance (0–604800 seconds), enabled
flag and delivery mode. Date-only occurrences need an explicit reminder time. These
private reminder resources synchronise with their computed `due_at` and occurrence
version so clients can schedule locally, including while offline.

<!-- experimental-schema: ReminderRule -->
```json
{"offset_seconds":-900,"late_seconds":3600,"enabled":true,"delivery":{"kind":"server"}}
```

`server` sends to the account's active subscriptions. `device` names one device responsible
for local delivery and creates no server job. This assignment is explicit and does not
switch automatically when a device or server loses connectivity. Clients must replace
scheduled notifications when the rule or occurrence version changes, cancel them on
completion/revocation, and deduplicate using the delivery ID. PWA/browser background
execution is not guaranteed: reliable offline scheduling needs a capable native client.
A disconnected device can retain information until its next sync, as with other content.

Subscriptions are private, versioned settings with transport `ntfy` or `web_push` and a
matching tagged `secret` object. ntfy accepts a topic URL and optional bearer token;
Web Push takes endpoint, p256dh and auth from the browser subscription. Creating a
subscription uses expected_version zero. Updates require its returned version. Removal
and device retirement deactivate the subscription and erase its encrypted secret.
At most 16 active subscriptions and 1,000 reminder rules are allowed per account.

The worker runs every 15 seconds, refreshes up to four due feeds (normally every 15
minutes), reconciles anchors and processes up to eight deliveries. Each claim has a
60-second fenced lease and is revalidated against current access, rule, occurrence and
subscription state before dispatch. Completion, archive or revoked access cancels pending
work. Expired reminders are retained as expired history. Retries back off from 30 seconds,
with at most eight attempts. A crash on the final attempt becomes failed after its lease.

Delivery is at-least-once: a crash after a provider accepts a message can cause a retry.
Retries retain the same ID; Web Push also uses it as Topic. Providers and clients may
collapse duplicates but Atlas cannot promise exactly-once display. Payloads contain only
a generic prompt and delivery identifiers, never private task or calendar text. Fetch
current authorised details through Atlas after opening the notification.

`GET /notification-capabilities` reports configured transports and the public VAPID key.
`GET /notification-subscriptions` excludes secrets; `/notification-deliveries` lists
current authorised delivery history. APNs and FCM are reported unavailable: direct native
provider interoperability remains conditional on credentials and real test clients.
Web Push uses actual VAPID signing and AES128GCM encryption; ntfy uses its HTTP publishing API.


## Configuration and operation

- `ATLAS_SECRET_KEY`: 64 hexadecimal characters encoding a 256-bit encryption key.
  Required for stored link/subscription settings; retain it with protected backups.
  Losing or replacing it makes existing encrypted settings unreadable. Re-enter settings
  after a deliberate key change; automatic key rotation is not implemented.
- `ATLAS_VAPID_PRIVATE_KEY`: base64url P-256 private key for Web Push.
- `ATLAS_VAPID_SUBJECT`: contact URI, such as `mailto:admin@example.org`.
- `ATLAS_OUTBOUND_ALLOW_ORIGINS`: optional comma-separated exact origins permitted to
  use HTTP or private addresses, for example a locally hosted ntfy service. This is an
  administrator-granted network exception; omit it for public HTTPS-only integrations.

Outbound connections resolve and validate every destination address, pin the accepted
addresses for the request, disable proxies and redirects, and have connect/request timeouts.
Private, loopback, link-local, multicast and reserved networks are denied by default.
Two concurrent integration fetch/validation operations are allowed per server process.
Connection data is encrypted with AES-256-GCM and bound to its resource ID. A keyed
fingerprint permits idempotent replay without storing plaintext receipts. Server logs
use generic failure codes and never include integration URLs, tokens or notification text.


Back up the integration encryption key with the database. Feed generations and
delivery leases persist across restart. Retention/export and multi-replica load
validation remain release work; see [roadmap](roadmap.md).
