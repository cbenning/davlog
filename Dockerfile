# Build
# Base images are pinned by digest so a build is reproducible; Dependabot proposes updates.
FROM rust:1-trixie@sha256:e84068000361bf3d4a5ba2b7647f62893528b1eb9f847d7a23b493f5ce8ea254 AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

# Run. libgit2 and libssh2 are built into the binary; only OpenSSL is dynamic.
FROM debian:trixie-slim@sha256:8aa5a7bf5a1955bddbe7d74fcdd0b91ab30e74362a8b73af672b0481b35b44a0
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl libssl3t64 \
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
