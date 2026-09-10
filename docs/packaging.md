# Release packaging

The `Release artefacts` workflow builds native Linux AMD64/ARM64 and macOS ARM64
archives, plus tested Linux container images, on tags and manual dispatch. It uploads
artefacts to the workflow run; it does not publish a GitHub release or push to a
registry. Review the corresponding Checks run and release evidence before publication.
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
