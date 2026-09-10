# Release packaging

The `Release` workflow builds native Linux AMD64/ARM64 and macOS ARM64 archives,
smoke-tested Linux AMD64/ARM64 container images and a Helm chart. Manual dispatch
uploads Actions artefacts only, even when run against a tag.

Pushing a tag such as `v0.1.0` publishes after all packaging jobs pass:

- A GitHub Release containing native archives, the Helm chart, `SHA256SUMS` and
  `container-image.txt` recording the multi-architecture image manifest.
- `ghcr.io/cyclikal94/atlas:v0.1.0`, containing both Linux architectures. The
  architecture tags `v0.1.0-amd64` and `v0.1.0-arm64` are also available.
- `oci://ghcr.io/cyclikal94/atlas/charts/atlas`, with chart version `0.1.0` and
  defaults pointing at the matching container image. Existing database Secrets
  are still required.

Tags must use `vMAJOR.MINOR.PATCH`, optionally followed by a SemVer prerelease
suffix such as `-rc.1`. Prerelease tags create GitHub prereleases. Build metadata
(`+...`) is not accepted, and no mutable `latest` container tag is published.
Review the corresponding Checks run and release evidence before pushing a tag;
the packaging workflow does not replace the full CI matrix or release gates.

Publishing uses the repository's `GITHUB_TOKEN`, with `contents: write` and
`packages: write` limited to the publish job. Ensure GitHub Actions is permitted
to publish packages; after the first publication, check that both GHCR packages
are public if anonymous pulls are intended. Existing packages must grant this
repository Actions access. Registry uploads and GitHub Releases are not one atomic
operation: a failed publication can leave registry images or a chart available.
Inspect that state before retrying; do not move an already published version tag.

Archives preserve executable permissions using tar and include checksums, revision,
lockfile hash, recovery tooling and configuration documentation. Dynamic OS libraries
are listed in `runtime-libraries.txt`; they must be installed on the target machine.
Linux archives built on Ubuntu 24.04 require a compatible glibc/OpenSSL environment;
use the Debian-based container for its pinned userspace. macOS archives target the
runner's OS/architecture and require the OpenSSL library reported by `otool`.

Build locally with `cargo build --release --locked -p atlas-server`, install
`cargo-about` 0.9.2 with `--locked --features cli`, then run:

```sh
scripts/package-release.sh aarch64-apple-darwin /tmp/atlas-release
```

Use the actual native build target. Package only a clean revision for publication;
a local package made from an uncommitted tree is a test artefact. Helm can be packaged
with `helm package deploy/helm/atlas`; supply existing database Secrets at deployment.

## Dependency notices

`about.toml` and `packaging/notices.hbs` generate notices from the locked server graph
for the build target. Dev-only dependencies are excluded; build dependencies are
included conservatively because some generate code embedded in the executable.
`cargo-about --fail` rejects unresolved licences. The generator can retrieve missing
upstream licence information, so builds need network access or a populated cache.
The template links exact crate source archives, including the unmodified MPL `ece`
source. Coverage checks retain the copyright text previously held in `third-party/`.
See [cargo-about configuration](https://embarkstudios.github.io/cargo-about/cli/generate/config.html)
for the generator's selection rules.

The image includes generated Rust dependency notices, Rust standard-library notices,
OpenSSL and CA-package copyright files, plus the Debian base's existing notices.
Native archives include Rust notices; dynamic system libraries remain OS packages.
`cargo deny` is an independent policy/advisory check and does not create distribution
notices. Review notice changes when updating dependencies; generation is not a substitute
for reviewing unusual upstream licensing or source-availability requirements.
