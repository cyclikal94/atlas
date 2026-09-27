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
opaque IDs, device labels, creation/expiry times, authentication method and a current
session flag. No token or token hash is returned. `DELETE /sessions/{id}` revokes only
that account's selected session. Each account may have up to 32 active sessions;
expired sessions are collected before admitting a new one.

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
| `confirmed_applied` | `200` | This operation retired the device: sessions and native handoffs deleted, subscriptions deactivated (secret erased), sync registration, cursors and delivery ledger removed. Content and account-wide command receipts survive. |
| `superseded` | `200` | Nothing was left to retire: every member was already gone. Nothing was changed, and this is **not** a confirmation. |
| `rejected_stale` | `409 operation_conflict` | The state changed after the user confirmed it (a session, handoff, subscription or registration was added, replaced or partly removed). Nothing was changed. |

The approved state has five components: live sessions, live native handoffs, active
notification subscriptions (by ID and version), pending activation grants (by ID) and the
sync registration (by a digest of its cursor key). A device whose only remaining member is
a pending grant — for example, a browser tab that obtained a grant but never redeemed it —
is still listed, with `summary.pending_sign_ins` counting it. Expired rows, inactive
subscriptions, `last_seen` and cursor churn are not state. A device that is retired and
recreated always has a different token. A user must confirm again, and the client must send a
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
It reads the ledger first, orders itself against sync-registration creation with a no-op
update of the account row, then re-reads every member, `FOR UPDATE` on PostgreSQL, so each row
it compares is locked until commit; on SQLite the write lock taken by the first statement
already excludes every writer. It then deletes, deactivates and cancels exactly the locked
identities, asserting each statement's rows-affected: any discrepancy means a writer escaped
the protocol, so the transaction rolls back with no record and the request fails `500`. A
removal by another cause is never credited to the retirement (or to a keyed revocation). The
transaction runs at PostgreSQL's default `READ COMMITTED`. Every writer of a member row must
either run inside `begin_serial()` or be ordered by its own row lock; a test lists every such
statement and fails when one is added or moved (see [testing](testing.md)).

Ledger rows are kept indefinitely, like receipts: an expiry would silently turn a confirmed
retirement into a hidden one after a long absence. They survive restore preparation, so a
recorded outcome still replays after a restore. Retirement and keyed revocation are online,
authenticated operations; cookie authentication requires the normal CSRF header. Device IDs
are account-scoped.

Per-device cursor keys are protected database state. See [operations](operations.md)
for the pre-release reset policy and [testing](testing.md) for provider test scope.
