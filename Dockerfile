# Build
# Base images are pinned by digest so a build is reproducible; Dependabot proposes updates.
FROM rust:1-bookworm@sha256:9181b56c2a21ab1d5e73cffe04fc762426d30aa7fac3c91141dd033f49a7fc8b AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

# Run. libgit2 and libssh2 are built into the binary; only OpenSSL is dynamic.
FROM debian:bookworm-slim@sha256:99c0b25c46d0214d5a2091092e7908907dcaecd6a962cde0ac700acee222b6f3
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /data --no-create-home davlog
COPY --from=build /src/target/release/davlog /usr/local/bin/davlog
# The repository lives here; a volume mounted here must be writable by uid 10001.
RUN mkdir /data && chown 10001 /data
USER 10001
VOLUME /data
EXPOSE 8080
# Matches `listen = "0.0.0.0:8080"` in the mounted config.
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s \
    CMD curl -fsS http://127.0.0.1:8080/healthz || exit 1
ENTRYPOINT ["/usr/local/bin/davlog"]
