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
generic text and opaque identifiers, never private task or calendar text. Fetch
current authorised details through Atlas after opening the notification.

`GET /notification-capabilities` reports configured transports and the public VAPID key.
`GET /notification-subscriptions` excludes secrets; `/notification-deliveries` lists
current authorised delivery history. APNs and FCM are reported unavailable: direct native
provider interoperability remains conditional on credentials and real test clients.
Web Push uses actual VAPID signing and AES128GCM encryption; ntfy uses its HTTP publishing API.

## Web Push payload

ntfy receives a generic prompt and the delivery ID. The plaintext of a Web Push message is
JSON. When `ATLAS_PUBLIC_ORIGIN` is configured it is a Declarative Web Push message
(W3C Push API) that also keeps the three identifiers at the top level:

<!-- experimental-schema: WebPushPayload -->
```json
{"web_push":8030,"notification":{"title":"Atlas reminder","body":"Open Atlas to view your reminder.","lang":"en-GB","dir":"ltr","tag":"3f2b8c1e-5a7d-4e90-b1c4-8d6e2a9f0b73","navigate":"https://atlas.example/?delivery_id=3f2b8c1e-5a7d-4e90-b1c4-8d6e2a9f0b73&reminder_id=a1d4e7f2-9c35-4b68-8e1a-5f3c7d2b9a40&occurrence_id=c8e5b3a9-2d71-4f06-9a84-1b7e6c0d3f52"},"id":"3f2b8c1e-5a7d-4e90-b1c4-8d6e2a9f0b73","reminder_id":"a1d4e7f2-9c35-4b68-8e1a-5f3c7d2b9a40","occurrence_id":"c8e5b3a9-2d71-4f06-9a84-1b7e6c0d3f52"}
```

One payload serves both paths. A browser that implements Declarative Web Push displays
`notification` without running service-worker code and opens `navigate` when the user
activates it. Any other browser delivers the same bytes to the service worker's `push`
event, where a handler can build the notification from `notification` or from the
identifiers. Without `ATLAS_PUBLIC_ORIGIN`, or if the message would exceed the
3,052-byte plaintext limit (only a very long origin can), Atlas sends just the three
identifiers, the `ReminderNotification` shape. `NotificationCapabilities.web_push` does not
depend on the origin.

- `title` and `body` are fixed generic text. They never contain task, calendar, account
  or device text, and the payload carries no account, device or subscription identifier.
- `tag` and `id` are the delivery ID. Retries keep it, and the plaintext of a retry is
  identical, so platforms and clients can collapse or de-duplicate repeats. Atlas still
  cannot promise exactly-once display.
- `navigate` is the public origin's root with `delivery_id`, `reminder_id` and
  `occurrence_id` as query parameters. The client reads them and fetches authorised
  details through Atlas; the identifiers alone grant nothing.
- Atlas sets no `mutable`, `silent`, `data`, `actions`, icon, badge or `app_badge` member.
- Under the Notifications standard, activating a notification that has a navigation URL
  navigates and does not fire `notificationclick`, so a client that records clicks must do
  so from the page it lands on.

Backend checks cover the payload, its encryption and the worker path. They do not show
how any browser displays or activates a notification; that needs real browsers and the
client's service worker.


## Configuration and operation

- `ATLAS_SECRET_KEY`: 64 hexadecimal characters encoding a 256-bit encryption key.
  Required for stored link/subscription settings; retain it with protected backups.
  Losing or replacing it makes existing encrypted settings unreadable. Re-enter settings
  after a deliberate key change; automatic key rotation is not implemented.
- `ATLAS_VAPID_PRIVATE_KEY`: base64url P-256 private key for Web Push.
- `ATLAS_VAPID_SUBJECT`: contact URI, such as `mailto:admin@example.org`.
- `ATLAS_PUBLIC_ORIGIN`: when set, Web Push reminders are Declarative Web Push messages
  whose `navigate` link is on this origin; when unset they carry only identifiers.
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
