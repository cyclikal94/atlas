# API

[OpenAPI](../api/openapi.json) is the sole machine-readable contract. The API is
pre-release; compatibility is not promised until a supported release. Application
routes retain `/api/experimental/v1`; `/health` and `/ready` are outside that prefix.

| Surface | Purpose |
| --- | --- |
| Sessions, browser sessions, OIDC, registration and devices | Account authentication and device lifecycle |
| `commands`, `access-commands`, `management-commands` | Atomic ordered resource changes, direct sharing, household/default/policy management |
| `task-commands`, `task-access-commands`, task and occurrence reads | Definitions, evidence, schedules, timers, dependencies, rotas and lists |
| `people-commands`, person detail, duplicate/merge preview and requests | Explicit account references, consent and privacy-preserving merging |
| Calendar commands, source import/refresh, events and anchors | ICS enrichment and review |
| Reminder commands, subscription/capability/delivery reads | Device-local or server-owned reminders |
| `sync` | Authorised immutable snapshots followed by incremental batches |

`info.version` in the contract changes whenever externally observable behaviour changes,
not only the schema: a status, a required header, a response body, a side effect or a
cookie the API sets. The combined 0.14.0 contract makes
the device-retirement headers required and its success status `200` with a recorded outcome,
adds the state token and summary to `GET /devices`, adds an optional operation ID to
`DELETE /sessions/{id}`, and removes `Set-Cookie` from every response that revokes the
caller's own session. The `0.15.0` contract replaces cookie-setting login, registration
and OIDC-callback responses with an activation grant, adds `POST
/browser-sessions/activate` (the only cookie-setting response) and `.../cancel`, adds
`session_id` to `BrowserSession`, and distinguishes `credential_mismatch` from
`forbidden`. `0.16.0` makes no wire-format change: it extends the recorded compatibility
baseline (below) to also probe grant-only browser login, `credential_mismatch`,
activation and cancellation, which `0.15.0` introduced but the baseline had not yet
recorded — the probes are a floor, and this closes a gap in it. `0.17.0` changes
`configure_source`'s `connection` field: omitting it (or sending `null`) now preserves
the source's stored connection instead of clearing it, and a new `disconnect` boolean
is the explicit way to clear it; sending both `connection` and `disconnect: true` is
rejected with `invalid_value`. A client built against the old "omission clears"
semantics will now unexpectedly preserve rather than clear a connection when it
intentionally omitted the field to disconnect — such a client must send
`disconnect: true` instead. `0.18.0` makes no further behavioural change: it documents
the `0.17.0` `connection`/`disconnect` semantics directly on those two properties in the
request schema. `0.19.0` integrates those calendar semantics with retirement cleanup for
already-inactive subscriptions: successful device retirement also erases their stored
secrets and advances their versions. The version advance is visible through
`GET /notification-subscriptions` and affects `expected_version` acceptance; the
compatibility baseline covers both the calendar and retirement behaviour together.
`0.21.0` combines two contract corrections. `ContentCommands`, `AccessCommands`
and `ManagementCommands` declare a maximum of 20 commands, matching the existing
implementation limit; clients may use that documented maximum rather than split
batches at eight. The explicit `ErrorCode::TemporarilyUnavailable` application error
now maps to `503 temporarily_unavailable`, as already documented for the affected
operations, instead of falling through to `500 internal_error`. The compatibility
baseline covers both corrections and preserves the earlier protocol probes.
`0.24.0` adds recipient-safe merge previews, durable sent-request/invitation
history and distinct calendar-refresh outcomes. These changes share one combined
contract and compatibility baseline.

A recipient can read `GET /people/requests/{id}/merge-preview` even when one
identity is hidden from them; the hidden identity and its fields are omitted.
Accepting a merge requires a fresh `recipient_preview_token`, with `409 conflict`
for a missing or stale token. Withdrawn/handled and expired requests remain
separately observable as `404` and `410`. Existing response receipts keep their
legacy fingerprints when the optional recipient token is absent, so a retry of
an already completed operation still returns its original receipt.

`GET /people/requests/sent` and `GET /invitations/sent` provide sender-scoped,
paginated history that survives operational-row cleanup, including authoritative
accepted, declined, withdrawn/revoked and expired states. Sent entries identify
their recipients by account ID and username, including when directory browsing
is disabled. Inbound invitation responses keep their existing shape. Durable
history is backfilled by database schema migration `1002 → 1003`.

Missing or undecryptable calendar connection details return
`422 connection_unavailable`; absent server encryption configuration returns
`503 integration_unconfigured`. Superseded or expired refresh leases return
`409 stale_refresh` with `reason` equal to `generation_changed` or
`lease_expired`. Either `refreshCalendar` or the successful-parse finish stage of
`importCalendar` can return that reason. Archived-source and access/existence
failures retain their own guarded outcomes. Parser-stage invalid dates and
oversized event summaries retain `502 fetch_failed`; their failure publication
updates health, settles the lease and permits a corrected immediate retry.

`GET /calendar-sources` includes a read-time `refresh_in_progress` boolean,
derived from the valid live lease and never persisted. It becomes false after
configuration invalidation or lease expiry even while an obsolete provider
response remains in flight. The compatibility probes assert these observations,
both stale-refresh reasons, the distinct refresh errors, recipient-preview
acceptance and sent-history behaviour alongside the existing protocol floor.

Mutations require an account-scoped UUID `Idempotency-Key` and explicit version
preconditions where defined. Commands that replace existing authored text also carry the
resource's sharing revision (`expected_policy_version`), so a write cannot commit under
sharing its author did not see; see [sharing](sharing.md). Repeat a logical operation with the same key/body after
an uncertain response. Reusing a key for different content conflicts. Receipts return
a committed revision, never cached protected content. Follow with authorised reads or
sync to obtain current state.

The atomic resource batch intentionally supports text-field creation/editing alongside
person creation. All fields use the same stored tagged value model; richer fields use
`put_field`. This batch preserves ordered identity-plus-contribution creation and
all-or-nothing application. Consent previews and online sharing commands remain
separate because they enforce different authority requirements.

`Projection.value` is native JSON, including on snapshot and delta responses. Clients
must not JSON-decode it again. Fields have a `kind` tag. Lists contain authorised task
references; hidden required progress produces unknown rather than a false aggregate.
Schema validation covers structure; domain services additionally enforce numeric,
timezone, ownership and consent rules.

Error responses contain a stable code, readable message and correlated request ID.
Expected application failures are typed internally. Unknown failures return a generic
internal error without database/provider content. See the contract for per-route
statuses and [sync](sync.md) for client recovery.

## API version and compatibility

`GET /health` returns the version of the API contract the running server was built with:

```json
{"status": "ok", "api_version": "0.24.0"}
```

`api_version` is exactly `info.version` in [the contract](../api/openapi.json), read from
that file when the server is built, so a binary cannot report a version other than the
contract it shipped with. It is strict `MAJOR.MINOR.PATCH` (numeric parts, no `v` prefix,
prerelease, build metadata or leading zeros). Compare it numerically as SemVer; `0.10.0`
is greater than `0.9.0`. A published release tag and its `@atlas/api-types` package
must have the same `MAJOR.MINOR.PATCH` as this contract. The tag has a leading `v`,
and a prerelease package may also have a suffix such as `-rc.1`; the crate version
is independent. Read `provenance.json.api_version` inside the types package for a
compatibility declaration, rather than inferring it from the package version. `/ready` and every session,
authentication and retirement response carry no version.

### When to advance the version

Advance `info.version` in the same change as:

- any edit to `api/openapi.json` other than `info.version`, including wording, because a
  description can state behaviour and no check can tell wording from meaning; and
- any change to externally observable behaviour, whether or not the contract text
  changes: validation rules and limits, defaults, ordering, pagination defaults,
  status and error-code selection, idempotency, receipt and replay semantics, side
  effects and sync publication, authentication, cookie, CSRF and session behaviour,
  headers a client relies on, rate limits or timeouts that change an outcome, and bug
  fixes a client could observe.

Refactors and performance changes with identical results, logging, tests, packaging and
prose outside the contract need no advance.

While the version is `0.y.z`, increase MINOR for anything that could change what an
already-built client or an already-queued command sends, receives or experiences:
removed, renamed, retyped or tightened members, any change to an existing response shape
or enum, changed validation, defaults, side effects or error mapping, and changes to
sync or authentication semantics. Increase PATCH for changes existing traffic cannot
observe: a new operation, a new optional request property or query parameter that
defaults to the previous behaviour, a schema no existing response references, or a
wording correction. The policy after 1.0 is decided with the first supported release.
Adding a required property to an existing response is a MINOR change.

A version names exactly one contract. Never reuse, reorder or lower one. Branches that
advance from the same base conflict on `info.version` and `api/compatibility.json` by
design: the later branch takes the next free version, re-records and reviews the diff.

### How this is enforced

- **Contract, mechanical.** `api/compatibility.json` records `api_version` and the
  SHA-256 of `api/openapi.json`. `scripts/validate_contract.py` and the server
  `compatibility` cases fail when the contract changed without a matching version and
  baseline.
- **Behaviour floor, mechanical.** The `compatibility` cases also drive the real router
  through a fixed set of probes (`/health`, `/ready`, an unauthenticated `/me`, and the
  HTTP status and code of every `ErrorCode`, content writes with omitted/stale/current
  sharing revisions, and stale/applied/replayed retirement outcomes followed by a rejected
  write through the retired session) and record the transcript in the baseline.
  A change to a probed behaviour fails until `info.version` is greater than the
  recorded one and the baseline is re-recorded. Record mode refuses to overwrite a
  changed baseline whose version has not advanced.
- **Review, human.** A change to externally observable behaviour must extend the probes
  to cover what it changes ([CONTRIBUTING](../CONTRIBUTING.md)).

The probes are a floor, not a proof. No automated check can show that unprobed behaviour
did not change, so the advance rule above applies to every change, not only probed ones.

To record after advancing the version:

```sh
ATLAS_RECORD_COMPATIBILITY=1 cargo test --locked -p atlas-server --test api compatibility
```

Do not edit `api/compatibility.json` by hand. Before merging a branch that has already
recorded a version, restore the baseline from the merge base, keep one advance from the
merge base's version and record again, so the branch introduces a single version.

### Client contract

The web client (or any client that queues changes) must:

1. Call `GET /health` without a session, cookie or CSRF header. It is unauthenticated,
   never sets a cookie and is sent `Cache-Control: private, no-store`; do not cache it.
2. Treat a missing `api_version` (an older backend), an unparseable value or a value
   outside its declared supported range as "compatibility unknown", which is
   unsupported. Absence is never compatible by default.
3. Compare as SemVer against its own declared range before offering sign-in, before
   automatically submitting queued changes and on reconnection, and pause automatic
   submission with a clear message when the check does not pass.
4. Never block the cached application shell or eligible offline display on this check.
   The version says nothing about whether a session is valid.

Each answering instance reports its own version, and instances may differ during a
rolling update, so a client re-checks rather than remembering one answer. The check
answers only for the instance that replied to `/health`. It does not bind the instance
that later handles a submission, and it cannot stop the server changing between the
check and the send.

This is a residual race that the `/health` protocol does not resolve. A client that
read `0.13.0` can still send a queued mutation that the server, now at `0.14.0`,
interprets and commits under the new behaviour; the same can happen between two replicas
during a rolling update. A response could report the version that handled it, but that
would be detection only: it arrives after the request has been processed, so it could
flag a mismatch for later requests and cannot prevent the current command being
committed under incompatible semantics. A lost response prevents even that late signal.
No such header is provided, and no response should be read as evidence that the
mutation was compatible. Preventing execution would need a separately designed
pre-execution compatibility condition, or a deployment and routing guarantee that
keeps a client's requests on a compatible version; neither exists today.

Compatibility is still not promised before a supported release.
