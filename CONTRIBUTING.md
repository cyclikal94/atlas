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

Preview the documentation site that GitHub Pages publishes from `main`:

```sh
python3.14 -m venv /tmp/atlas-site-env
/tmp/atlas-site-env/bin/python -m pip install -r scripts/requirements-site.txt
/tmp/atlas-site-env/bin/python scripts/build_site.py site
/tmp/atlas-site-env/bin/python -m http.server --directory site 8000
```

Then open `http://localhost:8000`. Serve the site rather than opening the files
directly; the API explorers fetch the description over HTTP. The pinned Markdown
release needs Python 3.10 or newer, and CI builds on the 3.14 shown above.

The index page is [nav.md](docs/nav.md): its heading and prose become the
introduction, and each `## ` section becomes a group of links, so the file also
serves as the index when reading `docs/` on GitHub. The build refuses a document
that file does not list, and a link that does not resolve, so give a new document
a place in that list.

The live smoke owns a disposable server/database. Container changes also require
`scripts/smoke_container.py IMAGE` on the built image; CI covers native ARM64/AMD64.
Store routine build logs in CI artifacts, not checked-in milestone directories.
Keep documentation about current decisions, behaviour, limits and unresolved work.
Archived development history retains superseded experiments and reviews.

This is unreleased software. Deliberate schema/wire changes may require an explicit
reset. Do not add compatibility for old experimental states or silently delete user
storage. A supported release will need a defined upgrade and restore policy.
