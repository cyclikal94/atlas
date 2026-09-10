# Architecture

Atlas is one Rust backend with two crates: `atlas-core` owns domain behaviour and
transactional storage; `atlas-server` owns HTTP, authentication and external
transports. Axum/Tokio serve requests and background work; SQLx accesses SQLite or
PostgreSQL. Rust provides explicit types and resource control on ARM64 and AMD64.
The repository does not claim a measured performance advantage over Go or TypeScript.
The toolchain and dependency graph are pinned and checked together.

Keep domain modules within this process. There is no Redis, message broker, plugin
ABI, feature-toggle framework or federation. Future features should reuse resource
policies, command receipts and sync publication rather than duplicate those systems.

## Transactions and storage

Writes use an explicit transaction gate on `sync_clock`. Content, policy changes,
receipts and sync publication commit together. Scheduling state and durable worker
claims use the same database. External network calls happen outside the transaction;
results are accepted only with the current generation/claim. Removing the global gate
requires measured evidence and replacement guarantees for publication ordering,
credential revocation, dependency cycles and retries.

SQLite and PostgreSQL are supported by the same individually named domain tests.
Relational columns hold identifiers, relationships, ownership and queryable time state;
JSON text stores validated domain payloads. API projections return native JSON.
Separate complete schemas retain each engine's constraints and hierarchy triggers.
There is no pre-release migration history. See [operations](operations.md).

Current limits include 100 accounts and 10,000 resources per installation. These are
explicit bounds, not claims about production capacity. The current-runtime workload
is `crates/core/examples/sync_load.rs`; prototype timings do not establish current
capacity. Release workload and two-replica testing remain in the [roadmap](roadmap.md).

## Domain boundaries

A resource is the common unit of identity, ownership, content version, policy version,
archiving and synchronisation. People, tasks, occurrences and progress have different
rules while sharing this infrastructure. Tasks represent one-off work, habits, chores,
quotas and event-relative work. Do not introduce a parallel habit model.

A visible person includes their name. Additional fields use one tagged value model
with independent policies. Task annotations use the same fields. Parent visibility is
required before a child can be seen; independently sharing a child cannot silently
reveal a private identity. Read/edit grants do not convey sharing administration.
Linked-account identity, contribution ownership and household membership are distinct.

Task occurrence identity follows intended saved wall-time slots, not the current UTC
clock. Clock changes, calendar disappearance, window closure and session expiry never
delete task records. Imported ICS recurrence follows its own explicit/generated time
rules. Completed work retains its historical interpretation. See [tasks](tasks.md).

Offline clients retain drafts separately from their authorised server cache. Content
operations can be retried; access changes require a connection. Defaults-revision
guards prevent delayed creation under unexpectedly broader sharing defaults. Sync
revocation can clear a cache only when the client reconnects; it cannot erase an
already downloaded fact from an offline device.

## Errors and observability

`ErrorCode` is the shared application failure vocabulary. HTTP maps typed failures to
status/code pairs in one module; arbitrary database/provider messages are never used
as public application codes. Request IDs correlate sanitised structured logs with API
errors. Provider settings and credentials are handled by transport/authentication
modules, never exposed through ordinary resource projections.
