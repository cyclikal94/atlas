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

## Deployment and recovery checks

After building the server, `python3 scripts/test_recovery.py` exercises real SQLite
exports, credential files, corruption rejection and occupied-destination protection.
`scripts/test-postgres-release.sh` owns a disposable PostgreSQL cluster and runs two
actual server processes, alternating domain HTTP requests between them, racing one
operation ID, measuring a bounded concurrent write sample, stopping one replica and
restoring an export into a second database. Set `ATLAS_PYTHON` to a Python environment
with `scripts/requirements-contract.txt` installed. Worker generation and retry fencing
are also tested through independent database pools in the calendar reconciliation cases.

`python3 scripts/test_deployment_templates.py` validates Helm constraints and Compose
configuration. It needs Helm and Docker Compose but no live daemon or cluster.
`python3 scripts/smoke_container.py IMAGE` uses disposable PostgreSQL containers and
checks generated notices, rejects SQLite/missing database configuration, and exercises
tasks, people, calendars, persistence and a bounded write sample.

Run `python3 scripts/test_compose.py IMAGE` for supplied/existing PostgreSQL and
restart checks in an isolated Compose project. For a disposable kind cluster, load
the image using `kind load docker-image IMAGE --name CLUSTER`, then run `python3 scripts/test_helm.py IMAGE --kubeconfig PATH`.
The test owns a unique namespace and removes it afterwards; it verifies supplied and
external PostgreSQL, two API replicas, health probes and restart persistence. Delete
the disposable cluster after testing. Never supply a personal/production kubeconfig.

Release archives and licence coverage are described in [packaging](packaging.md).

## Validation scope (10 September 2026)

M6 was checked on macOS 26.6.2 ARM64 with Rust 1.98.1 and PostgreSQL 17.11, and in a
Linux ARM64 VM allocated four CPUs and 6 GiB RAM. Both database suites passed: 68 core
cases (plus one deliberately ignored crash-process helper), 11 HTTP cases and six
server unit tests. Real export/restore, concurrent HTTP replicas, native archive
checksums/execution, generated notice coverage, Compose and Helm checks passed.
The Helm workload used its configured 512 MiB memory limit for each API/database pod.

The container smoke exercised tasks, people, calendars, receipt replay, restart
persistence and sanitised logs on Linux ARM64 and emulated AMD64. Its separate bounded
write sample created 100 people through eight concurrent clients against PostgreSQL:

| Runtime | Write p50 | Write p95 |
| --- | --- | --- |
| Linux ARM64 release image | 13.8 ms | 48.4 ms |
| Linux AMD64 release image under emulation | 14.3 ms | 53.4 ms |
| Two native macOS debug processes | 12.0 ms | 38.3 ms |

These short samples check concurrent execution; they are not capacity estimates or
architecture comparisons. The dataset is small, the profiles differ, and the host/VM
were shared with other validation work. Native Linux CI execution, sustained load,
production storage characteristics and upgrades from future supported releases remain
release gates. The container builds are identified by manifest digests
`c60b83da9098a5d3950dfc20d89fce9711b063140af820ea33b8b63a4d36ad54`
(ARM64) and `c3589277f424e78e3917cf116e22f788bbeac2366809b9356b0febce4b51c33d`
(AMD64); native package provenance records the M6 working tree based on `b63bb66` as dirty.
That original commit is preserved in the archived development history; these results
have not been relabelled with rewritten commit IDs.
