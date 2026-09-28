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
| Content writes rejected atomically when sharing changed since they were read, for owners and collaborators, on both engines; held-gate ordering of `replace_policy` and existing-field `put_field`; edits never re-addressed through a merged alias | `resources/policy_precondition`, `tasks/policy_precondition`, `storage/contention`, `people/workflows` | Server `policy_precondition`; smoke and replica runs, including overlapping narrowing and save |
| Household membership, defaults, invitations and exclusions; durable sent-invitation history across every state, its cascade-revoke on member removal and concurrent-response consistency; the defaults revision formula pinned by a golden value; the combined sharing snapshot (one consistent read of defaults, households and members, on both engines against real writers, with negative controls and static tripwires on the writers it depends on) | `households` (`households/snapshot`) | Onboarding HTTP routes; server `snapshot` (real routes and the real onboarding writer against a paused read) |
| Merge aliases, owner visibility, linked identity, consent and request replay; recipient-safe merge previews (hidden identities, staleness, expired/withdrawn/wrong-kind/ownership-shift edge cases) and durable sent-request history surviving operational purge | `people` | Sync and calendar anchor interactions |
| Device quota, cursor bounds, retention without content loss | `accounts`, `sync` | Sessions and HTTP sync |
| Atomic device retirement: approved-state token and snapshot listing, ledger-first replay, stale/superseded/applied outcomes, operation-ID reuse | `accounts/approved_state`, `accounts/retirement_ledger` | Server `devices`; real processes in `scripts/smoke/devices.py` |
| Retirement ordered against every writer of a member row, retry as a unit, the rows-affected assertion, READ COMMITTED and REPEATABLE READ | `accounts/retirement_coordination` (engine-specific schedules: revoke, sweeps, retention cleanup, registration create and `last_seen`, and the real `reminder_command` subscription set) | Server `devices` (ordinary revoke and `DELETE /sessions/current` route table) and server `writers` (the real routes for session issue, handoff create and consume, password change); two real PostgreSQL processes in `scripts/test_replicas.py` |
| Every device holding a live member is listed with a token, including one left with only a handoff or subscription | `accounts/approved_state` | Server `devices`; `scripts/smoke/devices.py` (seeded fixture rows, real server) |
| No unlisted or moved writer of an approved-state member | `accounts/writer_inventory` | — |
| Additive `1000 → 1003` upgrade chain, unknown versions, concurrent upgrade, durable sent-history backfill on the `1002 → 1003` step | `storage/migration` | `scripts/test_recovery.py` restores a 1000 bundle |
| Atomic receipts/publication, read/write contention and real process crash recovery | `sync`, `storage/contention` | SQLite and PostgreSQL execution |
| Clean startup/reopen, concurrent initialisation, incompatible-schema rejection | `storage/initialisation` | Both database engines |
| Foreign keys and failed-initialisation rollback | `resources/hierarchy`, `storage/initialisation` | SQLite connection pool; transactional DDL on both engines |
| ICS exceptions/cancellation, event reconciliation, private anchors, leap birthdays, connection preserve/replace/disconnect on partial `configure_source` updates, safe, distinguishable refresh-error codes and read-time refresh-in-progress | `calendars` | Calendar HTTP routes and integration worker |
| Authentication, CSRF, session revocation, OIDC signatures and replay | — | Server `browser`, `sessions`, `onboarding`, `oidc` |
| API version signal, contract hash and probed behaviour baseline | — | Server `compatibility`, `scripts/validate_contract.py` |
| Fetch address rules, secret scope, real encrypted push payloads, the Declarative Web Push envelope, its origin-gated fallback and retry-identical plaintext | — | Server `integrations`; the payload probe in `compatibility`. Not browsers: display, clicks and service-worker fallback belong to the web client's tests |

The server `compatibility` cases check `/health`'s `api_version` and compare the contract
hash and a probe transcript with `api/compatibility.json`. After a deliberate change with
a greater `info.version`, record with `ATLAS_RECORD_COMPATIBILITY=1 cargo test --locked -p
atlas-server --test api compatibility`; see [API](api.md#api-version-and-compatibility).
The probes are a floor, not a proof, and cover only what they record.

The unreleased migration-chain tests were deliberately removed with the clean
baseline; the single additive `1000 → 1001` step has its own case (`storage/migration`),
and earlier experimental schemas still require a reset. Runtime receipt replay, alias resolution, request-ID reservation, and
snapshot/delta recovery remain supported and tested. Similar unit, database and
HTTP checks exercise different boundaries; do not remove one merely because it
mentions the same feature.

## Deterministic schedules and test hooks

The retirement's concurrency cases force named interleavings with test-only hooks (the
`test-hooks` Cargo feature of both crates): `retire.before_begin`, `retire.after_ledger_read`,
`retire.after_locking_reads`, `retire.before_effects`, `retire.before_commit`,
`devices.between_reads`, `sharing_snapshot.between_reads` and, in the server,
`session_delete.after_identity`. A test arms a point, holds the transaction there, lets a
competing writer run, then releases it; a point can also run one injected statement on the
transaction's own connection, and PostgreSQL row locking can be switched off as a negative
control. The sharing snapshot has three more controls:
its household reads can be moved to a later snapshot (`split_snapshot_reads`), PostgreSQL can
run it at `READ COMMITTED` (`snapshot_read_committed`), and its own consistency check can be
turned off (`verify_snapshot`) so a control can see the tear the check otherwise refuses.
Registries belong to one `Store`, never the process.
The feature is enabled only through a self dev-dependency, so the call sites expand to nothing
and no point name reaches a normal or release build. Two consequences: `cargo tree -p
atlas-server -e no-dev,features` must not show it (plain `-e features` includes dev edges),
and `cargo test` leaves a hook-enabled `target/debug/atlas-server`. Build real-process evidence
with a fresh `cargo build --locked -p atlas-server` and no `cargo test` after it, and check
`strings target/debug/atlas-server | grep -c retire.after_ledger_read` prints `0`.

The two engines order a competing writer differently, so each schedule names the engine's
point: on SQLite `begin_serial()`'s first statement takes the single write lock, so a writer
can commit first only at `retire.before_begin`; on PostgreSQL a writer that does not take
`sync_clock` can also commit at `retire.after_ledger_read`, and a blocked writer is
identified by `pg_blocking_pids` naming the retirement's own backend. PostgreSQL also reruns
every schedule that can be raised with the retirement at `REPEATABLE READ` (`every_schedule_also_passes_at_repeatable_read`
and `replay_survives_device_recreation_at_repeatable_read` in core; `every_route_schedule_also_passes_at_repeatable_read` and
`every_real_writer_also_passes_at_repeatable_read` in the server), and each of those schedules asserts the level it
observed (`READ COMMITTED` unless raised), so a raise that did nothing fails. The writers run in the server crate are the real
routes (`POST /sessions`, `GET /oidc/callback`, `POST /oidc/native/exchange`, `POST /password`,
`DELETE /sessions/{id}`, `DELETE /sessions/current`, `POST /browser-sessions`, `POST
/browser-sessions/activate`, `POST /browser-sessions/activate/cancel`), not a stand-in
statement. These in-process
schedules prove the protocol's outcomes for the interleavings they force. They are not
release evidence: `scripts/smoke_server.py` and `scripts/test-postgres-release.sh` exercise
the release-shaped binary over real sockets, and the latter races retirements against
revokes and logouts, and activation-grant redemption against both a retirement of the
same device and a concurrent cancellation of the same grant, between two server
processes (`ATLAS_RACE_ROUNDS`, default 200).

Use `crates/core/examples/sync_load.rs` for current-runtime workloads, and
`crates/core/examples/retire_load.rs` (with `ATLAS_TEST_POSTGRES_URL` for PostgreSQL) for the
cost of a retirement, of the device listing, and of unrelated writers beside retirements, and
`crates/core/examples/sharing_snapshot_load.rs` for the payload and latency of the sharing
snapshot against separate `defaults` and `households` reads. Record the
revision, database, architecture, workload and resource limits when reporting capacity;
results from a different implementation do not establish current throughput.

## Deployment and recovery checks

After building the server, `python3 scripts/test_recovery.py` exercises real SQLite
exports, credential files, corruption rejection and occupied-destination protection.
`scripts/test-postgres-release.sh` owns a disposable PostgreSQL cluster and runs two
actual server processes, alternating domain HTTP requests between them, racing one
operation ID, racing device retirements against revokes and logouts, racing activation-grant
redemption against a retirement of the same device and against a concurrent cancellation of
the same grant, measuring a bounded concurrent write sample, stopping one replica and
restoring an export into a second database (the operation ledger must survive the restore,
and restore preparation must leave no activation grant behind). Set `ATLAS_PYTHON` to a Python environment
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

The API types package has its own suite: `npm test --prefix scripts/api-types` builds packages
from tiny OpenAPI fixtures in temporary git repositories, with negative controls for
non-deterministic generation, umask leaks, release gates, damaged assets and misattributed provenance.
`scripts/test_release_workflow.py`, run in the contract environment (it needs PyYAML), guards
the workflow configuration and runs each workflow's own generate and verify commands through
`npm run --prefix` in a scratch checkout, asserting that its upload path finds the tarball and
sidecar. Neither uses the network beyond `npm ci`, GitHub or a running backend,
so they show the code behaves as designed, not that a `Release` run has published a good asset;
only a real tagged release does that. See [packaging](packaging.md#api-types-package).

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
