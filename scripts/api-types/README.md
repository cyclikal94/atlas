# API type release generation

The backend owns the `openapi-typescript` generator, its exact pin in `package.json` and its
npm lockfile. The package it builds contains declarations only, with no runtime dependencies;
the frontend separately owns `openapi-fetch`. The package layout, version rule and
reproducibility claim are documented in [docs/packaging.md](../../docs/packaging.md#api-types-package).

Use the Node version in `.node-version`. A `--release` build enforces it, because the gzip bytes
depend on the Node build's zlib.

```sh
npm ci --ignore-scripts --prefix scripts/api-types
npm test --prefix scripts/api-types
node scripts/api-types/generate.mjs --version 0.0.0-bootstrap.$(git rev-parse --short=8 HEAD)
node scripts/api-types/verify.mjs target/api-types/atlas-api-types-0.0.0-bootstrap.*.tgz
```

| Command | Purpose |
| --- | --- |
| `generate.mjs --version <semver> [--output <dir>] [--release]` | Builds `atlas-api-types-<version>.tgz` and its `.sha256` in `target/api-types` |
| `verify.mjs <tarball> [--sha256-file <path>] [--version <semver>] [--release]` | Checks an asset independently of how it was built; `--release --version <semver>` also proves it came from this checkout |
| `npm test` | Fixture tests; no network beyond installing dependencies |
| `python scripts/test_release_workflow.py` | Guards both workflows and runs their generate and verify commands in a scratch checkout (needs PyYAML, git, node and the installed generator) |

`npm run generate --prefix scripts/api-types` runs from this directory, so pass `--output` an
absolute path; a relative one resolves here, not at the repository root. The workflows use one
absolute `API_TYPES_OUTPUT` for generating, verifying and uploading.

Exit status is `0` on success, `1` when a check fails and `2` for a usage error. `generate.mjs`
prints one JSON line, including `tar_payload_sha256` for comparing builds across toolchains.

Without `--release` the version must be `0.0.0-<label>` and the result is an unshipped
diagnostic package; a local build is not release evidence. `verify.mjs` treats such a package
the same way: it checks the contents and declarations but reports `"source_verified": false`
and does not compare the recorded commit, contract or lockfile with the checkout. To verify a
release asset, check out its tag and run `verify.mjs <tarball> --release --version <semver>`,
which compares the commit, the committed contract, the lockfile digest and the version. `--release` is used only by the
Release workflow for a pushed tag. It refuses unless the tracked working tree is clean, the
version's `MAJOR.MINOR.PATCH` equals the contract's `info.version` and Node matches
`.node-version`. The generator reads `api/openapi.json` from `HEAD`, so commit a contract
change before generating. Generation needs `git` and `npm` on a POSIX system.

Release CI validates the OpenAPI contract, runs these tests, generates and verifies the package
and attaches the tarball and its SHA-256 sidecar to the release. Generator upgrades change every
consumer's declarations: regenerate and diff `index.d.ts` against the previous release first.

The package version is the release tag. The API contract version is
`provenance.json.api_version`, the same value the server reports as `api_version` on
`/health`; use it, not the package version, in compatibility declarations.
