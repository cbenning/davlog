# Build
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

# Run. libgit2 and libssh2 are built into the binary; only OpenSSL is dynamic.
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /data --no-create-home davgit
COPY --from=build /src/target/release/davgit /usr/local/bin/davgit
USER 10001
# The repository lives here; mount a volume owned by uid 10001.
VOLUME /data
EXPOSE 8080
# Matches `listen = "0.0.0.0:8080"` in the mounted config.
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s \
    CMD curl -fsS http://127.0.0.1:8080/healthz || exit 1
ENTRYPOINT ["/usr/local/bin/davgit"]
