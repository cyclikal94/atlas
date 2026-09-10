# Testing Atlas

Run `cargo test --locked --workspace` for SQLite and pure domain cases. Run
`scripts/test-postgres.sh` for the same core and HTTP cases against PostgreSQL.
The script starts a disposable socket-only cluster and removes it on exit. Set
`ATLAS_PG_BIN` if PostgreSQL tools are outside PATH. CI may instead supply
`ATLAS_TEST_POSTGRES_URL` pointing to a disposable test database: cases create
unique schemas there, and the caller owns their cleanup. Never use a production
or personal database for tests.

Filter by domain, for example `cargo test -p atlas-core --test integration tasks::timers`
or `scripts/test-postgres.sh tasks::timers`. HTTP cases live in the server's `api`
target. Provider tests open loopback listeners. The ignored `crash_writer` case is
a subprocess helper invoked by the sync lifecycle test; do not run it directly.

## Invariant register

| Obligation | Core case directory | Additional boundary |
| --- | --- | --- |
| Stable saved timezone slots, gaps/folds, date-only recurrence, exact numeric goals | `tasks/domain` | HTTP command validation |
| Immutable occurrences/history, corrections, shared completion and streaks | `tasks/lifecycle`, `participation`, `joint`, `checklists`, `numeric` | Task HTTP routes |
| Atomic dependency consent and cycle prevention | `tasks/dependencies` | Task HTTP routes |
| Timer overlap, rejection recovery, durable completion successors and rotas | `tasks/timers`, `timer_recovery`, `successors`, `missed_recurrence`, `completion_worker`, `rotas` | Task HTTP routes |
| Parent visibility, independent policies and policy/content versions | `resources` | Authenticated cross-device HTTP sync |
| Household membership, defaults, invitations and exclusions | `households` | Onboarding HTTP routes |
| Merge aliases, owner visibility, linked identity, consent and request replay | `people` | Sync and calendar anchor interactions |
| Device quota/retirement, cursor bounds, retention without content loss | `accounts`, `sync` | Sessions and HTTP sync |
| Atomic receipts/publication, read/write contention and real process crash recovery | `sync`, `storage/contention` | SQLite and PostgreSQL execution |
| Clean startup/reopen, concurrent initialisation, incompatible-schema rejection | `storage/initialisation` | Both database engines |
| Foreign keys and failed-initialisation rollback | `resources/hierarchy`, `storage/initialisation` | SQLite connection pool; transactional DDL on both engines |
| ICS exceptions/cancellation, event reconciliation, private anchors, leap birthdays | `calendars` | Calendar HTTP routes and integration worker |
| Authentication, CSRF, session revocation, OIDC signatures and replay | — | Server `browser`, `sessions`, `onboarding`, `oidc` |
| Fetch address rules, secret scope and real encrypted push payloads | — | Server `integrations` |

The unreleased migration-chain tests were deliberately removed with the clean
baseline. Runtime receipt replay, alias resolution, request-ID reservation, and
snapshot/delta recovery remain supported and tested. Similar unit, database and
HTTP checks exercise different boundaries; do not remove one merely because it
mentions the same feature.

Use `crates/core/examples/sync_load.rs` for current-runtime workloads. Record the
revision, database, architecture, workload and resource limits when reporting capacity;
results from a different implementation do not establish current throughput.
