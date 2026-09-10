# Atlas

Tools for keeping your life organised and staying close to the people in it.

Atlas is an API-first Rust backend for tasks, habits, chores, shared progress, people
and calendar enrichment. Individual accounts share within and across households;
future clients can queue content offline and synchronise on reconnect.

The backend is **pre-release**. Direct host execution supports SQLite or PostgreSQL;
Docker, Compose and Helm deployments use PostgreSQL. Versioned backup/restore and
release packaging are available. See the [roadmap](docs/roadmap.md) for remaining
release gates. Breaking changes may require an explicit database reset;
startup never deletes an incompatible database. Licensed under [AGPL-3.0-only](LICENSE).

Build with `cargo build --locked -p atlas-server`. Run
`target/debug/atlas-server account alice`, enter a password on stdin and close stdin.
Then run `target/debug/atlas-server`. Defaults are a local `atlas.sqlite` database and
`127.0.0.1:3000`. See [operations](docs/operations.md) for database and Docker setup.

| Start here | Details |
| --- | --- |
| [Contributing](CONTRIBUTING.md) | Development commands and repository conventions |
| [Architecture](docs/architecture.md), [data model](docs/data-model.md) | Boundaries, decisions and storage relationships |
| [API](docs/api.md), [OpenAPI](api/openapi.json) | Current routes and wire contract |
| [Sharing](docs/sharing.md), [sync](docs/sync.md) | Privacy, defaults and offline recovery |
| [Tasks](docs/tasks.md), [people](docs/people.md) | Domain semantics and commands |
| [Calendars](docs/calendars.md), [notifications](docs/notifications.md) | Imported evidence and reminder ownership |
| [Authentication](docs/authentication.md), [operations](docs/operations.md) | Accounts, providers and deployment settings |
| [Configuration](docs/configuration.md), [recovery](docs/recovery.md) | Secrets, backups, restore and upgrade policy |
| [Testing](docs/testing.md), [roadmap](docs/roadmap.md) | Invariants and remaining release work |
