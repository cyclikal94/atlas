# API type release generation

The backend owns the `openapi-typescript@7.4.3` generator and its npm lockfile.
The declarations package has no runtime dependencies. The frontend separately owns
`openapi-fetch`. Use Node 22.23.2 or a compatible LTS release.

```sh
npm ci --prefix scripts/api-types
npm run generate --prefix scripts/api-types -- --version 0.0.0-bootstrap --output target/api-types
```

Release CI validates the OpenAPI contract first, generates declarations twice and
requires byte-identical output, then packages the exact schema and provenance with
the types. Release assets include the tarball and its SHA-256 checksum. Local
bootstrap packages are explicitly unshipped; a local build is not release evidence.
