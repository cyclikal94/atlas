# Vendored compatibility copies

Atlas builds two published crates from local copies through `[patch.crates-io]` in the
workspace `Cargo.toml`. Each copy is the crates.io archive unpacked unchanged, except for
its dependency manifest. No Rust source file is modified.

They exist because RUSTSEC-2026-0387 affects `crypto-bigint` 0.5.5. The only route to it
was `elliptic-curve` 0.13, which both crates pull in through the 0.13 curve stack. On
29 October 2026 neither crate had a release on the fixed stack. The copies move both
crates onto `elliptic-curve` 0.14 / `crypto-bigint` 0.7, and no 0.5 copy remains in
`Cargo.lock`. Scope and authority: BE-Q23 approval `BE-Q23-CRYPTO-SCOPE-20261029`. That
approval allows at most two such copies and limits each one to the dependency change named
below.
The BE-PEOPLE-ACTIVITY branch reproduces the same two copies, byte for byte, under its own
dependency-maintenance approval (2 November 2026), which sets the same limits.

| Copy | Upstream archive | SHA-256 (crates.io checksum) | Upstream commit | Licence | Change |
| --- | --- | --- | --- | --- | --- |
| `openidconnect/` | `openidconnect` 4.0.1 | `0d8c6709ba2ea764bbed26bce1adf3c10517113ddea6f2d4196e4851757ef2b2` | `b639b5d39eac6903238867aeb2b29326502e6b26` | MIT | `p256` 0.13.2 → 0.14.0, `p384` 0.13.0 → 0.14.0 |
| `web-push/` | `web-push` 0.11.0 | `d5c305b9ee2993ab68b7744b13ef32231d83600dd879ac8183b4c76ae31d28ac` | `8de73e2e6d3e56786561924dd132a76ea18ae042` | Apache-2.0 | `jwt-simple` 0.12.11 → 0.15.0 (`pure-rust`, no default features, as before) |

The only changed file in each copy is `Cargo.toml`, the normalised manifest Cargo reads.
Each change sits under a comment starting `# Atlas modification`. `Cargo.toml.orig` and
every other file match the archive byte for byte. That includes the archives' own
`Cargo.lock` files, which Cargo ignores for a patched dependency. They are tracked even
though each crate's `.gitignore` lists them.

## What stays the same

- **openidconnect:** OAuth2 5.0.0, `rsa` 0.9.10, `ed25519-dalek` 2, HMAC/SHA-2 0.12/0.10,
  `rand` 0.8 and the TLS features are unchanged. Every algorithm it verified before is still
  verified: RS/PS 256/384/512, ES256, ES384, EdDSA and HS 256/384/512. ES512 is still
  unsupported. The ECDSA code calls `VerifyingKey::from_sec1_bytes`,
  `Signature::from_slice` and `Verifier::verify`. Those calls compile unchanged against
  `p256`/`p384` 0.14, and nothing from the curve crates appears in its public API.
- **web-push:** the public API, HTTP-ECE (`ece`), the payload limit (3052 bytes), topic
  rules, TTL and VAPID claims are unchanged. Raw, PEM and DER key import is unchanged. The
  jwt-simple types it uses (`ES256KeyPair`, `JWTClaims`) live in private modules and are not
  re-exported, so its public API does not change.

## Behaviour notes recorded by tests

- A VAPID private key of the wrong length (not 32 bytes after decoding) used to panic inside
  jwt-simple 0.12 when the configuration was loaded. With 0.15 it is refused as
  `invalid VAPID key`, like the other invalid keys. Valid keys behave as before
  (`invalid_vapid_keys_are_refused`).
- Upstream openidconnect 4.0.1 panics on an HMAC signature of the wrong length. That path is
  not affected by this change. `oidc_signature_verification_matrix` records the panic.
- Upstream web-push builds the VAPID `aud` claim from scheme and host only, so it drops a
  non-default port. That code is not affected either.

## Toolchain and workspace

The manifests still declare their upstream `rust-version` (1.65 and 1.80). The fixed curve
stack itself needs Rust 1.87 or later. Atlas pins Rust 1.98.1 in `rust-toolchain.toml`, so
the Atlas toolchain does not change.

`vendor` is excluded from the workspace. As a result, Cargo does not resolve the crates'
dev-dependencies into Atlas's `Cargo.lock`, and workspace tests, lints and `cargo fmt --all`
skip these crates. Builds show their compiler warnings, because Cargo only silences
warnings for registry crates. These warnings come from unchanged upstream code. To run a
crate's own tests, copy it to a scratch directory and run `cargo test` there.

## Updating or removing

Check each new Atlas build for upstream releases of `openidconnect` and `web-push` on
`elliptic-curve` 0.14. On 2 November 2026, `openidconnect` 4.1.0 (published 31 October 2026)
was on the 0.14 curve stack. It is a new upstream release that also moves to `ed25519-dalek` 3
and `getrandom` 0.4 and needs Rust 1.88, so adopting it is a separate dependency decision.
`web-push` still had no such release. When both are available, update the pins in
`Cargo.toml`, remove the `[patch.crates-io]` entries and this directory, and drop the vendor
lines from the `Dockerfile`, `.dockerignore` and `packaging/notices.hbs`. To recheck a copy,
download `https://crates.io/api/v1/crates/NAME/VERSION/download` and confirm its SHA-256 matches the
table. Unpack it and compare it with the directory. The only difference should be the marked
lines in `Cargo.toml`.
