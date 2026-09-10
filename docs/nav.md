# Atlas Documentation

Tools for keeping your life organised and staying close to the people in it.

Atlas is an API-first Rust backend for tasks, habits, chores, shared progress, people and calendar enrichment. Individual accounts share within and across households; supporting clients which can queue content offline and synchronise on reconnect.

## API reference

- [Swagger UI](swagger.html) — Browse and try every route against the OpenAPI description
- [Scalar](scalar.html) — A reference-style read of the same description

## Design

- [Architecture](architecture.md) — Crate boundaries, decisions and the shape of the server
- [Data model](data-model.md) — Stored entities, identifiers and their relationships
- [API](api.md) — Route surface, versioning and the wire contract

## Domain

- [Tasks](tasks.md) — Tasks, habits and chores, with commands and semantics
- [People](people.md) — People, households and the connections between them
- [Calendar enrichment](calendars.md) — Imported calendars and the evidence they contribute
- [Reminders and notifications](notifications.md) — Reminder ownership and delivery
- [Sharing and households](sharing.md) — Privacy defaults and what crosses an account boundary
- [Offline writes and synchronisation](sync.md) — Offline queueing and reconciliation on reconnect

## Operating

- [Authentication](authentication.md) — Accounts, providers, sessions and tokens
- [Server configuration](configuration.md) — Settings, environment variables and defaults
- [Running Atlas](operations.md) — Databases, Docker and deployment
- [Export, backup and restore](recovery.md) — Backup, export and restore procedures
- [Release packaging](packaging.md) — Artefacts, dependency notices and publication checks

## Project

- [Testing Atlas](testing.md) — Invariants and how the suites are organised
- [Remaining release work](roadmap.md) — Remaining work before release
