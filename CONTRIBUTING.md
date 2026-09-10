# Contributing

Use the toolchain in `rust-toolchain.toml`; shared dependency versions live in the
workspace manifest. Keep `Cargo.lock` committed. Use UK English in documentation.

Core behaviour belongs in `crates/core`; HTTP/authentication/provider transport
belongs in `crates/server`. Reuse policies, typed fields, receipts and publication
for new domains. Group tests by behaviour, not milestone, bug reporter or review tool.
Keep transaction ownership visible when splitting modules.

During development run the affected domain cases, for example:

```sh
cargo test --locked -p atlas-core --test integration tasks::timers
scripts/test-postgres.sh tasks::timers
```

Before merging a substantive change:

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
scripts/test-postgres.sh
cargo deny --locked check
```

Provider tests use loopback listeners. The PostgreSQL runner starts and removes a
socket-only disposable cluster by default; see [testing](docs/testing.md) for overrides.
Use focused checks while editing and one complete final matrix, rather than repeatedly
rebuilding every target after small changes.

Validate changed API schemas and live traffic:

```sh
python3 -m venv /tmp/atlas-contract-env
/tmp/atlas-contract-env/bin/python -m pip install -r scripts/requirements-contract.txt
/tmp/atlas-contract-env/bin/python scripts/validate_contract.py
cargo build --locked -p atlas-server
/tmp/atlas-contract-env/bin/python scripts/smoke_server.py
```

The live smoke owns a disposable server/database; CI covers native ARM64/AMD64.
Store routine build logs in CI artifacts, not checked-in milestone directories.
Keep documentation about current decisions, behaviour, limits and unresolved work.
Archived development history retains superseded experiments and reviews.

This is unreleased software. Deliberate schema/wire changes may require an explicit
reset. Do not add compatibility for old experimental states or silently delete user
storage. A supported release will need a defined upgrade and restore policy.
