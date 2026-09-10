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

Mutations require an account-scoped UUID `Idempotency-Key` and explicit version
preconditions where defined. Repeat a logical operation with the same key/body after
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
