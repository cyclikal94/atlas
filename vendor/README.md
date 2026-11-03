# Vendored compatibility copies

Atlas builds two published crates from local copies through `[patch.crates-io]` in the
workspace `Cargo.toml`. Each copy is the crates.io archive unpacked unchanged, except for
its dependency manifest and, in `openidconnect` only, one Rust source file
(`src/core/jwk/mod.rs`, see [HMAC signature length](#hmac-signature-length)). No other
source file is modified.

They exist because RUSTSEC-2026-0387 affects `crypto-bigint` 0.5.5. The only route to it
was `elliptic-curve` 0.13, which both crates pull in through the 0.13 curve stack. On
29 October 2026 neither crate had a release on the fixed stack. The copies move both
crates onto `elliptic-curve` 0.14 / `crypto-bigint` 0.7, and no 0.5 copy remains in
`Cargo.lock`. Scope and authority: BE-Q23 approval `BE-Q23-CRYPTO-SCOPE-20261029`. That
approval allows at most two such copies and limits each one to the dependency change named
below. The later approval `BE-Q23-HMAC-TOKEN-SCOPE-20261103` adds only the HMAC length
correction in the `openidconnect` copy; it adds no copy and no other change.

| Copy | Upstream archive | SHA-256 (crates.io checksum) | Upstream commit | Licence | Change |
| --- | --- | --- | --- | --- | --- |
| `openidconnect/` | `openidconnect` 4.0.1 | `0d8c6709ba2ea764bbed26bce1adf3c10517113ddea6f2d4196e4851757ef2b2` | `b639b5d39eac6903238867aeb2b29326502e6b26` | MIT | `p256` 0.13.2 → 0.14.0, `p384` 0.13.0 → 0.14.0; HMAC `verify_slice` (below) |
| `web-push/` | `web-push` 0.11.0 | `d5c305b9ee2993ab68b7744b13ef32231d83600dd879ac8183b4c76ae31d28ac` | `8de73e2e6d3e56786561924dd132a76ea18ae042` | Apache-2.0 | `jwt-simple` 0.12.11 → 0.15.0 (`pure-rust`, no default features, as before) |

The changed files are `Cargo.toml` in each copy, the normalised manifest Cargo reads, and
`src/core/jwk/mod.rs` in `openidconnect`. Each change sits under a comment starting
`# Atlas modification` (manifests) or `// Atlas modification` (source). `Cargo.toml.orig` and
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

## HMAC signature length

Upstream `openidconnect` 4.0.1 verifies HS256/384/512 signatures with
`mac.verify(signature.into())`. The conversion to a fixed-size array panics when the
signature's length differs from the MAC output (32, 48 or 64 bytes), before any error is
returned. An ID token from a configured provider with an empty, short or long HMAC signature
therefore panicked the callback instead of being refused. Scope and authority: BE-Q23 approval
`BE-Q23-HMAC-TOKEN-SCOPE-20261103` (Review 12 finding H1), which allows exactly this narrow
correction and no other source change.

The three HMAC arms of `CoreJsonWebKey::verify_signature` now call the existing
`Mac::verify_slice(signature)` from the same `hmac` 0.12 / `digest` 0.10 stack. It returns
`MacError` for a wrong length and otherwise performs the same constant-time comparison, so
the existing `bad HMAC` error is returned for every invalid signature. Valid signatures,
the supported algorithms, key formats, claim checks and the public API are unchanged, and no
signature is truncated, padded or compared by Atlas code. The diff against the archive is
the three call sites and their `// Atlas modification` comments.

Tests: `oidc_hmac_known_answers_and_malformed_lengths` (RFC 4231 vectors at the exact
length; altered, empty, one-byte, short and overlong signatures refused),
`oidc_signature_verification_matrix` (independent `ring` signatures; any panic is a failure)
and `oidc_hmac_callbacks_refuse_malformed_signatures` (the real browser and native callbacks
refuse each malformed or invalid MAC with `401` and create no authority; valid controls sign
in). The earlier panic characterisation is preserved in the BE-Q23 review evidence.

## Behaviour notes recorded by tests

- A VAPID private key of the wrong length (not 32 bytes after decoding) used to panic inside
  jwt-simple 0.12 when the configuration was loaded. With 0.15 it is refused as
  `invalid VAPID key`, like the other invalid keys. Valid keys behave as before
  (`invalid_vapid_keys_are_refused`).
- An HMAC signature of the wrong length is refused as an invalid signature (see above).
- Upstream web-push builds the VAPID `aud` claim from scheme and host only, so it drops a
  non-default port. That code is not affected by this change.

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
`elliptic-curve` 0.14. When both are available, update the pins in `Cargo.toml`, remove the
`[patch.crates-io]` entries and this directory, and drop the vendor lines from the
`Dockerfile`, `.dockerignore` and `packaging/notices.hbs`. To recheck a copy, download
`https://crates.io/api/v1/crates/NAME/VERSION/download` and confirm its SHA-256 matches the
table. Unpack it and compare it with the directory. The only differences should be the marked
lines in `Cargo.toml` and, for `openidconnect`, the three marked HMAC calls in
`src/core/jwk/mod.rs`. An upstream release that refuses wrong-length HMAC signatures without
panicking makes the source change unnecessary; check that before dropping it.
