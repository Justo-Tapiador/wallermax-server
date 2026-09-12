# syntax=docker/dockerfile:1

# wallermax-server container image.
#
# Multi-stage build:
#   1. `builder`  — compiles the release binary with cargo; dependencies
#      are cached in their own layer so source-only changes rebuild fast;
#   2. `runtime`  — minimal Debian carrying only the binary, the default
#      configuration, the static assets and Node.js (the JHS sidecar's
#      runtime), running as an unprivileged user with the SQLite
#      database on a /data volume.
#
# Build:  docker build -t wallermax-server .
# Run:    docker run -p 8080:8080 -v wallermax-data:/data wallermax-server
#
# Every configuration value can be overridden with WALLERMAX_* environment
# variables (see README.md, "Environment variable overrides"), e.g.:
#   docker run -e WALLERMAX_AUTH__JWT_SECRET="$(openssl rand -hex 32)" \
#              -e WALLERMAX_TLS__ENABLED=true \
#              -v $PWD/certs:/app/certs:ro \
#              wallermax-server

# ---------------------------------------------------------------------------
# Stage 1: build the release binary
# ---------------------------------------------------------------------------
# 1.88 matches `rust-version` in Cargo.toml: the resolved dependency tree
# (Cargo.lock) needs it (edition2024 crates). Bump both together.
FROM rust:1.88-slim AS builder

# libsqlite3-sys (bundled SQLite) and ring (the rustls crypto provider)
# compile C code, and the slim image ships no compiler.
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Cache dependencies in a layer of their own: build a throwaway crate from
# the manifests first, then swap in the real sources below.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && touch src/lib.rs \
    && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src \
    && rm -f target/release/deps/wallermax_server* \
           target/release/deps/libwallermax_server* \
           target/release/wallermax-server

# Real sources: only the wallermax-server crate itself rebuilds now.
# (migrations/ is compiled in by the sqlx::migrate! macro.)
COPY . .
RUN cargo build --release --locked \
    && mv target/release/wallermax-server /wallermax-server

# ---------------------------------------------------------------------------
# Stage 2: minimal runtime
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

# ca-certificates keep outbound TLS working; Node.js runs the JHS
# sidecar (`[templates] backend = "auto"` by default — without Node the
# server silently renders on the in-process boa engine instead); the
# dedicated user owns the database directory so the server never runs
# as root. The media root (F9) is pre-created under /data alongside it:
# the server would create it at startup anyway, but baking it in means
# a fresh volume inherits the right ownership from the image.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates nodejs \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /app wallermax \
    && mkdir /data /data/media \
    && chown wallermax:wallermax /data /data/media

WORKDIR /app

# Binary + default configuration + static assets. Migrations travel inside
# the binary (embedded at compile time); refresh tokens included.
COPY --from=builder /wallermax-server /usr/local/bin/wallermax-server
COPY wallermax.toml /app/wallermax.toml
COPY public/ /app/public/
COPY views/ /app/views/
# require() modules (v0.9.0): the directory ships as a placeholder in the
# repo, so the COPY always has a context; mount real modules over it.
COPY modules/ /app/modules/
# The JHS sidecar (v0.10.0): the Node service the server spawns and
# supervises when [templates] backend is "sidecar" or "auto". It needs
# no npm install — the engine is vendored and dependency-free.
COPY sidecar/ /app/sidecar/
# Container-friendly defaults; each can be overridden with `docker run -e`.
#   * bind all interfaces so published ports work
#   * keep the SQLite database on the /data volume
#   * keep the media library on the /data volume too: uploads are site
#     content (like the database, unlike the static assets baked into
#     the image), and the default relative "media" would resolve under
#     /app — root-owned and read-only for the app user, so the F9
#     startup check (create the media root, fail fast) would kill the
#     container before it could bind the port.
#     Bind-mounting an empty host directory over /data skips the image's
#     ownership seeding: chown it to uid 10001 (the app user), the same
#     rule the SQLite file already follows.
ENV WALLERMAX_SERVER__HOST=0.0.0.0 \
    WALLERMAX_DATABASE__URL=sqlite:///data/wallermax.db?mode=rwc \
    WALLERMAX_CMS__MEDIA_DIR=/data/media

USER wallermax
VOLUME /data
EXPOSE 8080

# The binary is PID 1 and handles SIGINT/SIGTERM itself (graceful shutdown),
# so no init wrapper is needed. There is no built-in HEALTHCHECK either:
# probe GET /health (or the Prometheus endpoint) from your orchestrator.
ENTRYPOINT ["/usr/local/bin/wallermax-server"]
