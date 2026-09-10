FROM rust:bookworm@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922 AS build
WORKDIR /atlas
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
RUN rustup show active-toolchain
COPY crates crates
RUN cargo build --release --locked -p atlas-server
RUN mkdir /rust-notices && cp "$(rustc --print sysroot)/share/doc/rust/COPYRIGHT-library.html" /rust-notices/ && cp -R "$(rustc --print sysroot)/share/doc/rust/licenses" /rust-notices/

FROM --platform=$BUILDPLATFORM rust:bookworm@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922 AS notices
WORKDIR /atlas
COPY rust-toolchain.toml ./
RUN cargo install cargo-about --version 0.9.2 --locked --features cli
COPY Cargo.toml Cargo.lock about.toml ./
COPY crates crates
COPY packaging packaging
COPY scripts/notices.sh scripts/notices.sh
ARG TARGETARCH
RUN case "$TARGETARCH" in arm64) target=aarch64-unknown-linux-gnu;; amd64) target=x86_64-unknown-linux-gnu;; *) exit 1;; esac; scripts/notices.sh "$target" /THIRD_PARTY_NOTICES.txt

FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171
LABEL org.opencontainers.image.licenses="AGPL-3.0-only" \
      org.opencontainers.image.source="https://github.com/cyclikal94/atlas"
COPY LICENSE /usr/share/doc/atlas/LICENSE
COPY --from=notices /THIRD_PARTY_NOTICES.txt /usr/share/doc/atlas/THIRD_PARTY_NOTICES.txt
COPY --from=build /rust-notices /usr/share/doc/atlas/rust
COPY --from=build /usr/share/doc/libssl3/copyright /usr/share/doc/libssl3/copyright
COPY --from=build /usr/share/doc/ca-certificates/copyright /usr/share/doc/ca-certificates/copyright
# Web Push payload encryption uses the builder's pinned OpenSSL runtime.
COPY --from=build /usr/lib/*-linux-gnu/libssl.so.3 /usr/lib/
COPY --from=build /usr/lib/*-linux-gnu/libcrypto.so.3 /usr/lib/
COPY --from=build /atlas/target/release/atlas-server /usr/local/bin/atlas-server
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --chmod=755 packaging/atlas-container /usr/local/bin/atlas-container
ENV ATLAS_BIND=0.0.0.0:3000
USER 10001:10001
WORKDIR /
EXPOSE 3000
ENTRYPOINT ["atlas-container"]
