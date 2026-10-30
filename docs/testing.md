# Testing Atlas

Run `cargo test --locked --workspace` for SQLite and pure domain cases. Run
`scripts/test-postgres.sh` for the same core and HTTP cases against PostgreSQL.
The script starts a disposable socket-only cluster and removes it on exit. Set
`ATLAS_PG_BIN` if PostgreSQL tools are outside PATH. CI may instead supply
`ATLAS_TEST_POSTGRES_URL` pointing to a disposable test database: cases create
unique schemas there, and the caller owns their cleanup. Never use a production
or personal database for tests.
The PostgreSQL test role must be allowed to `SET deadlock_timeout` (the disposable
cluster and CI service use their test superuser). The deadlock schedule uses only
transaction-local settings to choose which participant detects the constructed
cycle, while still requiring a real `40P01`, a retry and exactly one ledger row.

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
| Independent per-occurrence timers (any arrival order, replay, concurrent starts, the unique index as a database-level backstop), rejection recovery, the account-wide timer list (order, keyset paging, redaction, `can_modify`), durable completion successors and rotas | `tasks/timers`, `timer_listing`, `timer_recovery`, `successors`, `missed_recurrence`, `completion_worker`, `rotas` | Server `timer_sessions` and the `independent_timers` compatibility probes; `scripts/smoke/timers.py` on a real server; two real PostgreSQL processes in `scripts/test_replicas.py` |
| Parent visibility, independent policies and policy/content versions | `resources` | Authenticated cross-device HTTP sync |
| Content writes rejected atomically when sharing changed since they were read, for owners and collaborators, on both engines; held-gate ordering of `replace_policy` and existing-field `put_field`; edits never re-addressed through a merged alias | `resources/policy_precondition`, `tasks/policy_precondition`, `storage/contention`, `people/workflows` | Server `policy_precondition`; smoke and replica runs, including overlapping narrowing and save |
| Household membership, defaults, invitations and exclusions; durable sent-invitation history across every state, its cascade-revoke on member removal and concurrent-response consistency; the defaults revision formula pinned by a golden value; the combined sharing snapshot (one consistent read of defaults, households and members, on both engines against real writers, with negative controls and static tripwires on the writers it depends on) | `households` (`households/snapshot`) | Onboarding HTTP routes; server `snapshot` (real routes and the real onboarding writer against a paused read) |
| Merge aliases, owner visibility, linked identity, consent and request replay; recipient-safe merge previews (hidden identities, staleness, expired/withdrawn/wrong-kind/ownership-shift edge cases) and durable sent-request history surviving operational purge | `people` | Sync and calendar anchor interactions |
| Device quota, cursor bounds, retention without content loss | `accounts`, `sync` | Sessions and HTTP sync |
| Atomic device retirement: approved-state token and snapshot listing, ledger-first replay, stale/superseded/applied outcomes, operation-ID reuse | `accounts/approved_state`, `accounts/retirement_ledger` | Server `devices`; real processes in `scripts/smoke/devices.py` |
| Retirement ordered against every writer of a member row, retry as a unit, the rows-affected assertion, READ COMMITTED and REPEATABLE READ | `accounts/retirement_coordination` (engine-specific schedules: revoke, sweeps, retention cleanup, registration create and `last_seen`, and the real `reminder_command` subscription set) | Server `devices` (ordinary revoke and `DELETE /sessions/current` route table) and server `writers` (the real routes for session issue, handoff create and consume, password change); two real PostgreSQL processes in `scripts/test_replicas.py` |
| Every device holding a live member is listed with a token, including one left with only a handoff or subscription | `accounts/approved_state` | Server `devices`; `scripts/smoke/devices.py` (seeded fixture rows, real server) |
| No unlisted or moved writer of an approved-state member, or of `device_profiles` (with negative controls for a rename outside `begin_serial()` and `record_client` reached from an unserialised caller); every statement that can list a device, the sync registration included, runs after the orphan prune (negative controls: a missing, comment-hidden or late prune and an unclassified member insert); every member and profile write holds the account's membership ordering point first, or takes `&Ordered`, or is a named exemption, and the device and session listings open `begin_membership_read()` before reading their time (negative controls: sign-out, sync registration and the registration sweep without it, the point moved after a handoff insert, the point no longer updating the account row, a retirement helper reached unordered, an unclassified writer, a listing on an unordered transaction or reading its time first, and an unlisted profile reader). Structural alarms only: the scheduled cases below prove the ordering | `accounts/writer_inventory` | — |
| Session/device client labels, device names and capacity (BE-Q25): the `User-Agent` classifier and name validator tables; labels from every session-issuing route (the browser label follows `activate`), several sessions per device, legacy `NULL`s; naming validation, `404` isolation, `401`/`403`, verbatim hostile text; names kept while listed (including across grant activation and handoff exchange of a device's only member), never in `state_token`, deleted by retirement and restore and pruned when the device is forgotten, whichever route lists it again (native and browser password, browser and native OIDC, a subscription change, a sync registration); overlapping membership changes in both orders, each with an extra-session control: a sign-out or unkeyed revocation against a browser grant and a sign-out against a first sync registration (removal first: seen absent, re-listed unnamed, the PostgreSQL sync retried once on a new snapshot; transition first: the removal and both listings wait, proved with `pg_blocking_pids` on PostgreSQL, and the name stays), a listing past the exact expiry of a session, handoff or grant waiting for a sign-in or replacement decided before it, the same transition decided at or after the expiry starting the device unnamed, a registration queued behind a listing deciding at its own later time, and the aged-registration sweep in both orders; capacity equal to both `device_capacity` checks at the boundary, with expired sessions and aged registrations; rename against a paused retirement in both orders | `accounts/device_names`, `accounts/device_quota`, unit tests in `devices.rs` | Server `labels` (unit tables in `labels.rs`, real routes in `cases/labels.rs`), the rename in server `writers`, the browser OIDC case in `oidc`, the `labels_and_capacity` probe group; real processes in `scripts/smoke/labels.py` (one SQLite process from `smoke_server.py`, two PostgreSQL processes from `test_replicas.py`) |
| Viewer-safe People summaries and original-creator attribution (BE-PEOPLE-SUMMARY): owner, household-member, direct-grantee and unrelated views; hidden grants, households and exclusions leaving a non-owner's bytes unchanged; `partial` for every non-owner; linkage, link/unlink, `reference_account` and merge attribution, including a link into an existing canonical profile, alias chains and merged known/unknown creators; unknown attribution in `shared_with_me`; effective field access following the person's narrowing, restoration, household joins/leaves and exclusions, and constant (`partial`) for the owner of a field on another owner's person; `partial` and unchanged for an owner of both outside any household the field or its person is shared with (removal and voluntary departure, rejoining, exclusion comparisons, parent-only, field-only and mixed households, typed and merged fields, mixed pages), with qualification and counts from one snapshot (paused between them, in both orders) and a failure between them returned as an error; `preferences_version` revealing a primary household change and a delayed same-epoch response; primary-household In Home; visible-only field counts and field audiences (PPL-6 exclusion); complete keyset pagination of every filter against an independent reference; revocation, archive and alias resolution; attribution inert for every write; one snapshot per page (paused between the id query and its enrichment, `REPEATABLE READ` on PostgreSQL); a source guard that every person insert records attribution, with negative controls | `people/summary` | Server `people_summary` (routes, `401`, headers, parameters, identical `404`s, no `403` beside `GET /policies/{id}`'s, unchanged `listPeople`/`getPerson`, a second-device primary household change, a removed owner's field audiences unchanged by native and browser registrations into the household, every body checked against the contract) and the `people_summaries` probe group; real processes in `scripts/smoke/people_summary.py` (one SQLite process from `smoke_server.py`, two PostgreSQL processes from `test_replicas.py`) |
| Additive `1000 → 1006` upgrade chain rooted on frozen genuine 1003, 1004 and 1005 schemas (every pre-existing value of every table unchanged, compared over the columns each table had before the upgrade; index, column and constraint parity with a fresh database), unknown versions, concurrent upgrade, durable sent-history backfill on the `1002 → 1003` step, `NULL` labels and an empty `device_profiles` after `1004 → 1005` and its `CHECK`s, an empty `person_attributions` after `1005 → 1006` (pre-existing people read `unknown`) and its keys; `scripts/backup.py` accepting exactly the schemas the chain can produce | `storage/migration`, the `SUPPORTED_SCHEMAS` test in `storage/mod.rs` | `scripts/test_recovery.py` and `scripts/test-postgres-release.sh` restore bundles of every accepted schema and start a real server on the result |
| Atomic receipts/publication, read/write contention and real process crash recovery | `sync`, `storage/contention` | SQLite and PostgreSQL execution |
| Opt-in bounded `/sync` delivery (BE-Q23): non-opt-in snapshot and delta responses byte-identical to a golden transcript captured on the base implementation; pieces of a split revision reassembling to the whole batch (mixed kinds, ancestor closure, removals, the deepest three-ancestor chain at `max_changes = 4`) at bounds 4 to 800; packing, isolation and `limit`; `min(limit, m)` snapshot pages; the `d1 → d2 … → d1` chain with exact retries and an unchanged ledger; every continuation field authenticated in its own MAC domain and bound to account, device and revision (expiry, retirement, pruning); continuation positions sealed so that tokens from different bounds or repeats cannot be compared to count hidden notices; per-piece authorisation (hidden before its slice: never sent, no removal; delivered then hidden: removed); convergence under edits, grants, revocations, household joins and departures between pieces, with a live-applying control showing why clients stage; the real 200-change dense grant and the synthetic 800-change control | `sync/golden`, `sync/segments` | Server `sync` (parameter validation, repeated parameters, member sets) and the `bounded_sync` probe group; `scripts/sync_dense_fixture.py` (server-side delivery profile on real processes, not client memory evidence) |
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
baseline; each additive step of the `1000 → 1006` chain has its own case
(`storage/migration`, rooted on the frozen `tests/fixtures/schema_1003_*.sql`,
`schema_1004_*.sql` and `schema_1005_*.sql`, never on the current schema), and earlier experimental schemas still require a reset. Runtime receipt replay, alias resolution, request-ID reservation, and
snapshot/delta recovery remain supported and tested. Similar unit, database and
HTTP checks exercise different boundaries; do not remove one merely because it
mentions the same feature.

## Deterministic schedules and test hooks

The retirement's concurrency cases force named interleavings with test-only hooks (the
`test-hooks` Cargo feature of both crates): `retire.before_begin`, `retire.after_ledger_read`,
`retire.after_locking_reads`, `retire.before_effects`, `retire.before_commit`,
`devices.between_reads`, `sharing_snapshot.between_reads`, `people_summary.between_reads` and, in the server,
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
snapshot against separate `defaults` and `households` reads, and
`crates/core/examples/people_summary_load.rs` for People summary page latency per filter
(with merged aliases), the per-page statement count, the field-audience page of a 99-field
person (effective access for every recipient) and the PostgreSQL plans of the filter
predicates, and
`crates/core/examples/timer_list_load.rs` for the account-wide timer list (pages over 1k to
100k finished sessions, the access-check cost, query plans with and without the
`timer_account_stopped` index) and, with `ATLAS_LOAD_MODE=migrate`, the `1003 → 1004` upgrade
of a database holding up to a million timer rows. Record the
revision, database, architecture, workload and resource limits when reporting capacity;
results from a different implementation do not establish current throughput.

## Sync diagnostic fixture (BE-Q23 R3)

`atlas-sync-diagnostic` is a separately named executable for real continuation expiry (X0) and
real pruning (X2) evidence over HTTP. It is built only with the non-default `sync-diagnostic`
feature (`required-features`), so `cargo build -p atlas-server` and the release and image builds
never produce it, and a normal build compiles neither it nor `crates/server/src/diagnostic.rs`.
Its only edit to normal source is an item appended to `resource_routes.rs` that, in a diagnostic
build, shadows the system clock for `/sync`; a normal debug build is byte-identical to one from
the previous commit at the same path.

```text
cargo build --locked -p atlas-server --features sync-diagnostic --bin atlas-sync-diagnostic \
  --target-dir target/sync-diagnostic
atlas-sync-diagnostic --fixture-dir NEW_ABSOLUTE_DIR --attempt ID --bind 127.0.0.1:0 \
  --public-origin ORIGIN
```

`--public-origin` is required. It configures the normal browser-session composition exactly as
`ATLAS_PUBLIC_ORIGIN` does for the normal server, with the same validation: browser grants,
activation (the only route that sets the cookie), `/browser-sessions/current`, cookie names and
flags, and the Origin and CSRF checks are unchanged. Give the origin the client is actually served
from (a pair runner's preview origin such as `http://127.0.0.1:PREVIEW_PORT`), not the fixture's
bind address. It refuses any inherited `ATLAS_*` variable, a non-loopback or non-literal bind
address, an invalid origin and an existing or relative fixture path. It creates the directory
(mode 0700), an empty SQLite file and `receipt.json` (schema 2: attempt, process, directory and
database file identities, executable SHA-256, features, API version, address, canonical public
origin, sync start, retention), prints `ready` with that receipt, then serves the normal router.
Retention is the production default of 90 days. There is no periodic maintenance or integration
worker, no HTTP control route and no way to open another database. The sync time starts at the
system time; authentication, publication timestamps and every other route keep the system clock.

Every HTTP request and every command is admitted only while the directory, database file and
receipt are still the ones the fixture created (by file identity and receipt digest, refusing
symbolic links). A request is checked on arrival and again once it holds the gate; a command
before it waits for the gate, once it holds it and, for `age_history`, once it holds the SQLite
write lock. The first loss is terminal: restoring the files does not revive the attempt. A refused
request is answered `503 {"code":"diagnostic_fixture_unavailable"}` before any extractor,
authentication or handler runs, and stops the attempt even while stdin is idle (`ownership_lost`,
then `stopped` with no cleanup). A request admitted before the loss may complete; its effects are
not attributed to the attempt. Nothing is answered as an owned success once ownership is lost:
the answer is `ok: false` with `ownership_lost: true`, no result and the command's `effect`.
Admission phases are logged to stderr as `diagnostic_http` and `diagnostic_control` events.

Commands are JSON lines on stdin, `{"attempt", "seq", "op", "args"}`, with `seq` counting from 1;
each accepted command is answered on stdout. A wrong attempt, stale or skipped `seq`, unknown
member or malformed line is `rejected` and consumes nothing. A line over 8 KiB, EOF, or a replaced
directory, database or receipt stops the attempt. Operations: `inspect` (floors, batches with
payload digests, devices with key digests, sessions, receipt and resource digests);
`add_account` (setup only); `age_history` (setup only, once: at most 16 batches named by account,
revision and payload SHA-256, back-dated no further than `sync_now - retention`);
`advance_clock` (forward only, at most twice the retention past the start); `collect` (the real
`Store::collect_expired` at a time between the start and `sync_now`, answered with the states
before and after and the removed batches; on failure it reports both states as `partial`, with
`after: null` and `after_error` if the state after it cannot be read); and `shutdown`
(`remove: true` deletes only the files the fixture created, then the directory if it is empty,
and never after a loss). The first `advance_clock` or `collect` ends setup. Every command except
`shutdown` takes the gate exclusively, so it waits for every in-flight HTTP request, not only
`/sync` (at most 10 seconds), and holds new ones back; a clock acknowledgement therefore means
earlier admitted requests used the old time and later ones use the new. Every answer carries
`effect`: `none`, `applied`, `partial` (the collector failed after possibly committing earlier
units; compare the states) or `unknown`. Every sync-time read is logged to stderr as
`diagnostic_sync_time`.

`crates/server/tests/sync_diagnostic.rs` drives the executable through this protocol: X0 and X2
(with a separate unpruned account) through both bearer and browser-cookie sessions, the browser
composition and its refusals, launch and command refusals, HTTP and command ownership admission
(including after waits and terminal loss), the quiescent boundaries and a partial collector
outcome. Its schedules are ordered by the logged admission phases and by holding the fixture
database's SQLite write lock, never by sleeps. Run it with `cargo test
-p atlas-server --features sync-diagnostic --test sync_diagnostic`. `SYNC_DIAGNOSTIC_BIN` selects a
separately built executable (the Cargo-built one also has `test-hooks` through the dev
self-dependency), and `SYNC_DIAGNOSTIC_EVIDENCE=DIR` records every control line and HTTP exchange.
Using a diagnostic build for browser pairing needs its own reviewed provenance and approval;
these backend results are not browser recovery evidence.

## Deployment and recovery checks

After building the server, `python3 scripts/test_recovery.py` (with
`scripts/requirements-contract.txt` installed) populates a SQLite database through a real
server, then exercises real exports, credential files, corruption and unknown-schema
rejection, occupied-destination protection (an existing or empty file is refused and left
byte for byte unchanged) and restore. Row digests of the durable tables, including the
timer history and both sent histories, must match across the restore; a real server then
starts on the restored file, serves the same history and accepts a new concurrent timer.
Bundles of every older schema, built from the frozen 1003 baseline, restore and upgrade
in place, and a server started directly on a 1003 database migrates it at startup.
`scripts/test-postgres-release.sh` owns a disposable PostgreSQL cluster and runs two
actual server processes, alternating domain HTTP requests between them, racing one
operation ID, racing device retirements against revokes and logouts, racing activation-grant
redemption against a retirement of the same device and against a concurrent cancellation of
the same grant, racing timer starts released together from both processes (different
occurrences must both start; one occurrence must have exactly one winner), measuring a bounded
concurrent write sample, stopping one replica and restoring an export into a second database.
Through `scripts/test_recovery_postgres.py` the restore is then compared row for row with the
source (timer history in both states, both sent histories and the operation ledger, with non-zero
counts), a refused restore into the occupied database must leave it unchanged, real servers on
the source and the restored database must serve identical history, and older bundles, a real
startup upgrade and the unknown-schema refusals are exercised (restore preparation must leave no
activation grant behind). Set `ATLAS_PYTHON` to a Python environment
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
