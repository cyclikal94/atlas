# Authentication

Native clients use `POST /api/experimental/v1/sessions` and send the returned opaque
bearer token in `Authorization`. Tokens expire after 24 hours; the database stores
only their SHA-256 hashes. Logging out deletes the server session. Authentication
expiry never deletes application records or task history.

## Browser sessions

Set `ATLAS_PUBLIC_ORIGIN=https://atlas.example`. Atlas accepts HTTP only for loopback
origins during local development. The origin must contain no path, user information,
query or fragment. TLS may terminate at a reverse proxy. Set the public origin to the
external URL; forwarded headers never determine cookie security or redirect targets.

`POST /api/experimental/v1/browser-sessions` takes the same credentials as native login
plus an `attempt_challenge` (the caller's S256 PKCE code challenge over a locally
generated `verifier`, never sent to the server) and requires an `Origin` header exactly
matching the configured origin. It sets **no cookie**. A successful password check
returns a one-time `BrowserGrant` (`grant`, `grant_expires_at`, `account_id`) instead:
the grant expires after 60 seconds and is what gets redeemed, not the credential check
itself, so a delayed or duplicated login response can never leave a stale session
behind. `POST /browser-registration` follows the same shape, returning a grant after
atomically redeeming the invitation (see "Invitation-based registration" below).

`POST /api/experimental/v1/browser-sessions/activate` with `{"grant","verifier"}` is
the **only** response in the contract that sets a cookie. It checks that
`S256(verifier)` equals the grant's stored challenge, then sets an HttpOnly, Secure,
SameSite=Lax `__Host-atlas_session` cookie with Path=/ and no Domain attribute (local
HTTP uses `atlas_session` without Secure) and returns `account_id`, `expires_at`,
`csrf_token` and `session_id`. A wrong verifier is `401` and does not redeem the grant;
it survives up to four wrong verifiers before the fifth also cancels it. Every grant is
single-use: activating an already-redeemed or already-cancelled one is `401`. A device
already at its 32-session capacity leaves the grant untouched and still redeemable
until it expires, so a retry after that failure can still succeed. `POST
/browser-sessions/activate/cancel` with `{"verifier"}` abandons an attempt a tab is no
longer pursuing (for example, a second tab winning the race, or the user navigating
away): it needs no grant reference and sets no cookie, and reports `not_activated`
(issued-then-cancelled, or expired but not yet swept), `session_revoked` (already
redeemed: only its session is revoked, the grant record stays as evidence) or `unknown`
(no matching record, including one already past its 48-hour retention window).

Send `X-CSRF-Token` with **every** subsequent cookie-authenticated API request,
including reads. The token is derived from the random session credential using a
domain-separated SHA-256 digest; its exposure does not reveal the bearer credential.
Comparison uses the OIDC dependency's timing-resistant secret equality implementation.
Any supplied Origin must also match. An explicit Authorization header takes priority;
an invalid bearer header cannot fall back to a cookie. A well-formed but wrong CSRF
token is `403 credential_mismatch`; an absent or malformed one is `403 forbidden`; a
missing, expired or already-revoked cookie is `401`.

After a page reload, `GET /api/experimental/v1/browser-sessions/current` with
`X-Atlas-Session: 1` returns the account, expiry, CSRF token and `session_id`. This
bootstrap does not itself require a CSRF token. The custom header requires a preflight for
cross-origin JavaScript, and Atlas grants no CORS access. Keep the eventual client
on the configured origin. `DELETE /api/experimental/v1/sessions/current` deletes the
session row of the credential it carries, and nothing else. No response that revokes the
caller's own session (logout, revoking your own session, changing your password, unlinking
a provider or retiring your own device) sets or clears a cookie: the row's deletion already
revokes the credential, so later use of it returns `401`, whereas a cookie decision made from
state read at request time but delivered later could clear the cookie of a newer login.
Changing your password, unlinking a provider and retiring a device also cancel every
`issued` activation grant of the account (or, for device retirement, of that device) in the
same transaction, so a grant minted before the change can never still be activated
afterwards; a plain logout or revoking one specific session does not, since neither
invalidates every session of the account. A client discards its own copy when it learns
the session has ended. Responses use private/no-store and no-referrer headers.

## OpenID Connect

Configure `ATLAS_OIDC_ISSUER`, `ATLAS_OIDC_CLIENT_ID`, optionally
`ATLAS_OIDC_CLIENT_SECRET`, and the public origin. Register the exact callback URI:
`https://atlas.example/api/experimental/v1/oidc/callback`. HTTPS is required for provider
traffic; HTTP is accepted only when the configured issuer and requested endpoint are
loopback URLs, for development/testing. Requests never follow redirects and have a
10-second timeout and 512 KiB response limit. Discovery happens during authentication,
so a provider outage does not prevent server startup or local login. Four provider
operations may run concurrently per process; authentication source budgets also apply.

Start with `POST /api/experimental/v1/oidc/start`, the matching Origin header and
`{"device_id":"browser","attempt_id":"...","attempt_challenge":"..."}` (the same
opaque attempt ID and S256 PKCE challenge over a locally generated `verifier` that
browser login uses). Navigate the browser to the returned `authorization_url`.
Atlas uses the authorisation-code flow with S256 PKCE, a nonce, random state and a
separate HttpOnly browser-binding cookie. State is persisted in the shared database,
expires after ten minutes and is consumed atomically before exchanging the code.
A new start replaces the browser-binding cookie, so only the latest flow in that
browser can finish. At most 1,000 pending flows are retained. Maintenance removes
expired flows; nonce/verifier values must be protected as authentication database data.

The library verifies ID-token signatures, issuer, audience, expiry and nonce; Atlas
also checks `at_hash` when present. Identity is the exact issuer/subject pair. Names,
usernames and email claims never link accounts automatically. Unknown subjects are
rejected unless the operator explicitly enables `ATLAS_OIDC_AUTO_PROVISION=true`.
That setting trusts the configured provider to admit new accounts. Provisioned
accounts receive generated usernames and have no local password. The current
experimental 100-account limit still applies.

To link an existing account, start with `"link":true` and a local or browser session
created within the last five minutes. The original session must still be valid when
the callback completes. An identity already linked to another account is rejected;
a second identity from the same issuer cannot silently replace the first. A successful
browser callback issues an activation grant, exactly as browser login does, and
redirects to the configured origin with `#atlas_grant=...&atlas_attempt=...&atlas_account=...`
in the URL fragment — never sent to the server or logged — instead of setting a
cookie directly. The fragment is `application/x-www-form-urlencoded` (as `URLSearchParams`
expects), so the caller-supplied `attempt_id` round-trips exactly even when it contains
`&`, `+`, `%` or other reserved bytes; decode it with a matching parser rather than
splitting on `&`/`=`. Redeem it with `POST /browser-sessions/activate` using the `verifier`
that produced the start request's `attempt_challenge`. There is no caller-selected
redirect target. Pending flows also bind the issuer, client ID and callback
configuration, so changes cannot reinterpret an old flow. Native OIDC uses the handoff below.

## Native OIDC handoff

Set `ATLAS_OIDC_NATIVE_REDIRECT_URIS` to a comma-separated list of exact callback
URIs. An empty list disables native OIDC. Allowed URI forms are HTTPS, loopback HTTP
or an application scheme containing a dot (a reverse-domain scheme such as
`dev.atlas.app:/oauth/callback`). User information, query strings and fragments are
rejected. URI matching includes the path and port; there are no wildcards.

The native client generates an S256 PKCE challenge/verifier pair and random client
state, retains the verifier/state locally, and opens this URL in the **system browser**:
`/api/experimental/v1/oidc/native/start?device_id=...&redirect_uri=...&code_challenge=...&state=...`.
Encode each query value. Atlas creates a browser-bound provider flow and redirects
to the provider. On success, the callback redirects to the allowed application URI
with `code` and the original client `state`. The native client must check that state
against its initiating login, then call `POST /oidc/native/exchange` with `code` and
`code_verifier`. This returns the ordinary Atlas bearer-session response.

Handoff codes last 60 seconds, are stored only as hashes, and are consumed atomically
with session creation. Wrong verifiers do not consume the code. Replay and expiry are
rejected. The code also binds provider/client/callback configuration, device ID and
the still-allowed native return URI. Password changes/recovery revoke pending codes.
At most 1,000 unexpired handoffs are retained; maintenance removes expired state.
The provider flow has its own, separate PKCE verifier. No bearer credential appears
in browser history or an application redirect URI. Native login does not change an
existing browser Atlas session. Explicit account linking uses the authenticated
browser start endpoint described above.

This has been tested through the API with a local provider and simulated native
client. Actual iOS/Android application URI association remains future client work.

## Invitation-based registration

Household managers can issue signup invitations with
`POST /api/experimental/v1/account-invitations`, providing `household_id`. Operators
can issue a standalone signup invitation with `atlas-server invite`. Both return an
opaque invitation ID, a secret token **once**, and expiry seven days later. The
database stores only a token hash. Deliver the token yourself; Atlas does not send
email or messages. Keep tokens out of query strings and logs.

`POST /registration/preview` with `{"token":"..."}` returns the household identity
and expiry so a client can show what is being accepted. `POST /registration` takes
the token, username, password and device ID. It atomically consumes the invitation,
creates the account, joins the household as a member, makes it primary and issues a
native session. Standalone invitations join no household. Use `/browser-registration`
with a matching Origin and an `attempt_challenge` to receive an activation grant
instead of a native session; redeem it exactly as browser login does. These paths
share the same account service and password-work limits, and the same atomic
invitation-redemption guarantee: only what happens after the account is created
(session vs. grant) differs.

New members' first snapshots include existing household-shared records, subject to
ordinary exclusions and parent visibility. No existing account can redeem a signup
invitation; invite it through the existing membership-invitation API instead.
A failed transaction preserves the invitation, including a conflicting username.
Only one concurrent redemption succeeds. A retry after a lost successful response
returns unauthenticated; log in using the credentials chosen during registration.
This online credential operation is deliberately outside the offline content-command
receipt protocol.

`GET /account-invitations` lists active invitations issued by the current account or
for households it manages, without tokens. `DELETE /account-invitations/{id}` revokes
an invitation under the same authority. Operators can use
`atlas-server revoke-invite ID`. At most 20 unredeemed, unrevoked, unexpired tokens
exist per issuer (including the operator issuer). Losing household manager rights
permanently revokes that account's pending signup invitations for the household;
regaining the role does not reactivate them. Redemption also rechecks current issuer
authority. Expired invitation metadata is removed after a further seven days.

## Current account and identity removal

`GET /api/experimental/v1/me` returns the account ID, username, whether a local
password exists, and linked OIDC issuers. It never returns credential hashes or
provider tokens. `DELETE /oidc/identities` with `{"issuer":"..."}` removes that
account's issuer link. This requires a session created within five minutes and a
local password, so it cannot remove the account's last login method. It revokes all
account sessions and pending native handoffs. Re-authenticate with the local password.

## Sessions and password recovery

`GET /api/experimental/v1/sessions` lists the current account's active sessions with
opaque IDs, device IDs, each session's client label and its device's name (see
[session and device labels](#session-and-device-labels)), creation/expiry times,
authentication method and a current session flag, plus `session_capacity`. No token or
token hash is returned. `DELETE /sessions/{id}` revokes only that account's selected
session. Each account may have up to 32 active sessions; expired sessions are collected
before admitting a new one (see [session and sync capacity](#session-and-sync-capacity)).

`POST /api/experimental/v1/password` takes `current_password` and `new_password`
(12–1,024 bytes). It verifies and hashes outside the async executor, then atomically
checks the original credentials/session, changes the password and revokes **all**
account sessions. Log in again afterwards. Login also rechecks the verified password
hash inside its transaction, preventing a concurrent password reset from being
followed by a session issued against the old password.

Operators can recover an account with `atlas-server password USERNAME`, supplying
the new password on stdin. This also revokes all account sessions. OIDC-only accounts
can establish a local password through this operator recovery path. No email recovery
or unauthenticated password-reset endpoint is exposed.

Schema version 4 migrates existing sessions to opaque IDs and derives creation time
from their original 24-hour expiry. Stop old server processes before upgrading;
older versions cannot issue sessions using the new schema. Version 5 adds signup
invitations, native OIDC handoffs and the corresponding lifecycle rules. Back up first.

## Session and device labels

Since API `0.28.0` (schema `1005`) the server records a coarse **client label** with every
session it issues, so a person can recognise their sign-ins, and lets them **name** their
devices. Both are display metadata for the signed-in owner only.

**Client label** (`ClientLabel`: `channel`, `browser`, `platform`). `channel` comes from the
route, never from a header: `browser` for `POST /browser-sessions/activate` (the only route
that sets the session cookie) and `native` for the three bearer-issuing routes
(`POST /sessions`, `POST /registration` and `POST /oidc/native/exchange`). `browser` and
`platform` are derived from that request's `User-Agent` alone; `browser` is `null` exactly
when `channel` is `native`. For a browser sign-in the label follows the `activate` request,
not the earlier login, registration or OIDC request that issued the grant. The header is
self-reported and trivially changed, so a label is a recognition hint and never identity,
proof of a device or an input to any security decision. Only the resulting closed values
are stored: no raw header, browser or OS version, device model, client hint or address, so
the stored data adds nothing to fingerprinting beyond about fifty coarse combinations of a
header the browser already sends. A header that is absent, repeated, longer than 512 bytes
or not printable ASCII gives `unknown`; metadata never blocks or fails a sign-in.

The first matching rule wins, on substrings of the header. Platform: `CrOS` → `chromeos`;
`Android` → `android`; `iPhone`, `iPad` or `iPod` → `ios`; `Windows` → `windows`;
`Macintosh` or `Mac OS X` → `macos`; `Linux` or `X11` → `linux`; otherwise `unknown`.
Browser: `Edg/`, `EdgA/` or `EdgiOS/` → `edge`; `OPR/` → `opera`; `SamsungBrowser/` →
`samsung_internet`; `Firefox/` or `FxiOS/` → `firefox`; `CriOS/`, `Chrome/` or `Chromium/` →
`chrome`; `Safari/` together with `Version/` → `safari`; otherwise `unknown`. Known
limitations: iPadOS in desktop mode reports macOS, Chromium-based browsers that do not
identify themselves (Brave, Vivaldi) report `chrome`, and any client can send anything. New
values may be added in a MINOR version; clients treat an unrecognised value as `unknown`.

Each session keeps the label it was issued with, so several sessions on one device can
differ. A device's `client` in `GET /devices` is the label of its most recently issued
session, and survives that session's expiry while the device is listed.

**Device name.** `PUT /api/experimental/v1/devices/{device_id}/name` with
`{"name": "..."}` sets a name and `DELETE` on the same path clears it; both return `204`.
Any live session of the account may name any of the account's devices, and duplicate names
are allowed. A device that is not listed for the caller's account (never seen, retired,
every member expired, or another account's) is `404 not_found` and nothing is written. A
name is 1–64 Unicode scalar values (so at most 256 UTF-8 bytes) with no leading or trailing
white space and none of: control characters (C0 and C1), the bidirectional controls U+061C,
U+200E, U+200F, U+202A–U+202E and U+2066–U+2069, the invisible U+200B, U+2060–U+2064, U+FEFF
and U+FFF9–U+FFFB, the separators U+2028 and U+2029, or any noncharacter. ZWJ and ZWNJ stay
allowed for emoji sequences and scripts that need them. A rule violation, or a device ID
outside 1–100 bytes, is `422 invalid_value`; a body that is not an object with exactly one
string member `name` (including `null`) is `400 malformed_request`. Nothing is normalised
or trimmed: the stored and returned value is exactly what was accepted. Text that looks like
markup is accepted as text; clients must render names as text, never as HTML. Every session
in `GET /sessions` also carries its device's current name as `device_name`.

A name is identification for the owner's own use, not a secret, but it may be personal (for
example a person's first name), so it is disclosed only to sessions of the same account and
is never written to logs. Device IDs are scoped to the account: the same device ID under two
accounts has two independent names and labels.

**Lifetime.** A device's name and latest label live in `device_profiles` and are visible
exactly while the device is listed, continuously: once a listing has seen a device absent, the
device starts unnamed, with no latest label, whenever it is listed again. They are deleted when a
retirement of the device commits `confirmed_applied` (a stale or superseded retirement keeps
them), when a restore is prepared, and, for every device of the account that is no longer
listed, by the next transition that can list a device: each of them first drops the profiles of
the account's unlisted devices, before it writes the new member. Those transitions are session
issue (password and registration sign-in, browser activation, native OIDC exchange), browser
activation-grant issue (password, registration and OIDC), native OIDC handoff issue, a
notification-subscription change (whose `device_id` is the client's) and a sync registration
(the first sync of a device, or its first after the registration was removed). A device that has
been forgotten therefore starts unnamed whichever route lists it again, including while it is
listed only by a pending sign-in or handoff. Redeeming a grant and exchanging a handoff replace a
member of the device that is live at the redemption's own time with its session, so that device
is listed throughout and keeps its name, even if the consumed grant or handoff was its only
member; a grant or handoff that had already expired by then keeps nothing. A sync request that
authenticated before its session was signed out still succeeds, and still registers its device,
but the earlier authentication is not evidence that the device stayed listed: if a listing saw it
absent in between, it is registered unnamed. A password change or OIDC unlink revokes every
session but keeps the names of devices that remain listed (for example through their sync
registration). Sessions and devices from before `1005` have `client: null`, and every device
starts with `name: null`; nothing is backfilled because no source data exists. The fallback
wording for a missing label or name ("This browser", "Another device" and similar) is the
client's.

**Ordering.** A name is not a member of the approved device state and is not part of
`state_token`, so renaming never makes a retirement confirmation stale. Each account has one
*membership ordering point*, its account row. Every change to the account's device membership
holds it from before its first member write to its commit: every member write above, every
removal (sign-out, revocation with or without an `Idempotency-Key`, grant cancellation including
the fifth wrong verifier, a password change, an operator reset, an OIDC unlink, a subscription
change, a retirement and the aged-registration sweep), and every profile write (a rename or clear
included). Each reads the time it judges liveness by only after taking it, on every retried
attempt. The `GET /devices` and `GET /sessions` listings take it too, shared, before they read
their time and snapshot, so they wait for a membership change of the same account in flight and
it waits for them; listings of other accounts, and other kinds of request, are not held up on
PostgreSQL. A decision that a device is still listed, and keeps its name, therefore stays true
until the new member is visible: a removal that commits first is seen by it, a removal that comes
later finds the new member, and a listing that has seen the device absent read its time before
the decision did, when the member that would have justified keeping the name had already gone or
expired. Removing a member never deletes a profile itself: the profile of a device left with no
member stays until the next transition of the account drops it, and is never shown in between.
Renaming is therefore totally ordered with retirement and with every membership change: a rename
either commits before a retirement, which then still applies and deletes the name with the
device, or runs after it and finds nothing listed (`404`). Expired sessions and handoffs swept
by maintenance are not ordered this way: they had already stopped being members at the sweep's
time.

On PostgreSQL the ordering point is a row lock on the account, held to commit; a listing holds it
in share mode, so it is a REPEATABLE READ snapshot that is no longer `READ ONLY`, and a membership
change committed after its snapshot started makes it retry. The first sync registration takes it
inside the sync's REPEATABLE READ transaction, so a removal committed after that snapshot started
fails the attempt with a serialisation error and it is retried whole with a new snapshot. On
SQLite the point is the database's single writer slot, which every writer already holds, so a
listing now holds that slot for its own short transaction too. The order is always
`begin_serial()` (where used), then the account row, then member rows; nothing takes them in the
reverse order. One limitation remains: the servers' clocks are assumed to agree, since each
decision reads its own server's clock.

## Session and sync capacity

Two per-account limits are enforced, and each is reported beside the list it governs as a
`Capacity` object `{"limit", "used", "remaining"}`. `limit` is the enforced limit, or `null`
when none is enforced (not produced by this release); `remaining` is `max(limit - used, 0)`,
and `null` exactly when `limit` is `null`. The limits are fixed by the server, are the same
for every account and are not configurable; clients must use the reported value and never
assume one (the design's example of five is not the limit).

- `GET /sessions` → `session_capacity`: live sessions, limit 32. `used` is the number of
  sessions in the same response, which is exactly what a new sign-in counts after it discards
  expired sessions; an expired session is counted by neither. A sign-in when `remaining` is
  `0` fails with `503 device_capacity` (a browser grant stays redeemable until it expires).
  Signing out, revoking a session, a password change and a retirement free slots when they
  commit.
- `GET /devices` → `sync_capacity`: sync registrations, limit 32, from the same snapshot as
  the device list. `used` counts every registration of the account, including one older than
  the retention period that cleanup has not yet removed, exactly as a sync from an
  unregistered device counts them. A device that is already registered needs no new slot; a
  new device's first sync when `remaining` is `0` fails with `503 device_capacity` until a
  retirement or the cleanup frees one. A device that only has sessions does not use a sync
  slot.

There is no limit on devices as such: the list in `GET /devices` can contain devices that
hold neither a session nor a registration (a pending sign-in, a native handoff or a
subscription). A count is a reading, not a reservation: another sign-in or registration can
take the last slot between the read and the client's own request, so a client must still
handle `device_capacity`. A client that cannot obtain a count (an older server without the
member, or a failed read) shows capacity as unavailable rather than inventing one; the server
never returns a partial answer.

## Dependency advisory assessment

`openidconnect` 4.0.1 depends on `rsa` 0.9.10, which is covered by
[RUSTSEC-2023-0071](https://rustsec.org/advisories/RUSTSEC-2023-0071), a timing side channel
that can disclose **private** RSA keys. The advisory has no patched release as checked
on 7 September 2026. Atlas does not load RSA private keys, decrypt with RSA or sign
with RSA in its application code. It uses provider public keys to verify signatures;
there is no Atlas RSA private key to recover in that path. `deny.toml` therefore
contains one explicitly justified exception. This is an assessment of Atlas's usage,
not a claim that the dependency is fixed. Reassess the exception before introducing
any private-key operations and when upgrading the dependency.

The local provider test uses a deliberately public, test-only RSA fixture to sign
synthetic tokens. It is not an application/deployment key and must never become one.

## Device retirement

`GET /api/experimental/v1/devices` lists every device of your account that holds any member of
the approved state (a sync registration, a live session, a live native handoff, an active
notification subscription or a pending activation grant), including the current device, last
sync time (or null) and active session count. Each device also carries the *approved state* a user could confirm
retiring, as an opaque `state_token` and a `summary` (sessions, native handoffs, notification
subscriptions, pending sign-ins, whether a sync registration exists) read together from one
database snapshot, so what a confirmation dialog shows is exactly what the token covers.

Each device also carries its `name` and latest `client` label and the response carries
`sync_capacity` (see [session and device labels](#session-and-device-labels)). The name and
label are display metadata, not members of the approved state: they are never part of
`state_token`, and a `confirmed_applied` retirement deletes them with the device.

The listing set is the union of the same member predicates the token covers, so a device
that can be retired always offers a token. In particular, a device whose last session has
gone but which still holds a live native handoff or an active subscription stays listed
with a fresh token and a `sessions` count of `0`; that is how a client that has just
received `rejected_stale` reads the state it must confirm again. A device appears until
nothing live remains: once every member is gone or expired it is no longer listed (and a
retirement of it would be `superseded`).

### Retiring a device

`DELETE /api/experimental/v1/devices/{device_id}` requires an `Idempotency-Key` (a
canonical lower-case UUID, the *operation ID*) and the `Atlas-Device-State` header holding
the token the user confirmed. It retires the device only if its complete approved state
still equals that token, and records the outcome durably in the same transaction:

| Outcome | Status | Meaning |
| --- | --- | --- |
| `confirmed_applied` | `200` | This operation retired the device: sessions and native handoffs deleted, subscriptions deactivated (secret erased), sync registration, cursors and delivery ledger removed, and the device's name and label forgotten. Content and account-wide command receipts survive. |
| `superseded` | `200` | Nothing was left to retire: every member was already gone. Nothing was changed, and this is **not** a confirmation. |
| `rejected_stale` | `409 operation_conflict` | The state changed after the user confirmed it (a session, handoff, subscription or registration was added, replaced or partly removed). Nothing was changed. |

The approved state has five components: live sessions, live native handoffs, active
notification subscriptions (by ID and version), pending activation grants (by ID) and the
sync registration (by a digest of its cursor key). A device whose only remaining member is
a pending grant — for example, a browser tab that obtained a grant but never redeemed it —
is still listed, with `summary.pending_sign_ins` counting it. Expired rows, inactive
subscriptions, `last_seen` and cursor churn are not state, though a `confirmed_applied`
retirement also erases the secret on every subscription tied to the device, active or not,
as uncounted cleanup — the same treatment cursor churn already gets. A device that is
retired and recreated always has a different token. A user must confirm again, and the
client must send a
**new** operation ID, after a `rejected_stale` outcome: it is never retried against the
changed state.

The outcome is recoverable. A self-retirement revokes the caller's own session, and a
response can be lost, so the client repeats the identical call (same ID, device and token)
under any other valid session of the account; the recorded outcome is returned without
evaluating anything again, even after the device has since been recreated. A repeat under
the revoked credential is `401`, which is not an answer. Reusing an ID for a different
device or token is `422`. An ID with no record is unresolved: absence of a device, an
expired session or a missing subscription is never evidence that a retirement happened.
`DELETE /api/experimental/v1/sessions/{id}` records the same way when it carries an
optional `Idempotency-Key` (`confirmed_applied` or `superseded`, never `rejected_stale`);
without one it remains a plain revocation (`204`, or `404` when nothing matched). A keyed
revocation credits only a live session row that this call removed.

Correctness under concurrency does not rest on client checks. The retirement runs in one
serialised transaction, retried as a unit on `SQLITE_BUSY` or PostgreSQL `40001`/`40P01`
(exhaustion is `503` with no record and no effect, so the client repeats the identical call).
It reads the ledger first, takes the account's membership ordering point (a no-op update of
the account row, see *Ordering* above), which orders it against sync-registration creation and
every other membership change, then re-reads every member, `FOR UPDATE` on PostgreSQL, so each row
it compares is locked until commit; on SQLite the write lock taken by the first statement
already excludes every writer. It then deletes, deactivates and cancels exactly the locked
identities, asserting each statement's rows-affected: any discrepancy means a writer escaped
the protocol, so the transaction rolls back with no record and the request fails `500`. A
removal by another cause is never credited to the retirement (or to a keyed revocation). The
transaction runs at PostgreSQL's default `READ COMMITTED`. Every writer of a member row must
either run inside `begin_serial()` or be ordered by its own lock (the account row, or the
member row itself for the maintenance sweeps of expired rows); a test lists every such
statement and fails when one is added or moved (see [testing](testing.md)).

Ledger rows are kept indefinitely, like receipts: an expiry would silently turn a confirmed
retirement into a hidden one after a long absence. They survive restore preparation, so a
recorded outcome still replays after a restore. Retirement and keyed revocation are online,
authenticated operations; cookie authentication requires the normal CSRF header. Device IDs
are account-scoped.

Per-device cursor keys are protected database state. See [operations](operations.md)
for the pre-release reset policy and [testing](testing.md) for provider test scope.
