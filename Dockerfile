# syntax=docker/dockerfile:1.7

# Build stage: compile the kiki binary against glibc.
# rusqlite (bundled) and mlua (vendored) compile their own C deps; TLS is
# rustls on every target, so no system OpenSSL is needed at build time.
FROM rust:1-trixie AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Pre-build dependencies as a cacheable layer. We stub out src/ so cargo
# resolves and compiles every dep without needing the real sources.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
    && echo 'fn main() {}' > src/main.rs \
    && echo '' > src/lib.rs \
    && cargo build --release --locked --bin kiki \
    && rm -rf src

# Now build the real binary. Touching the entry points forces cargo to
# rebuild the crate without re-resolving deps.
COPY . .
RUN touch src/main.rs src/lib.rs \
    && cargo build --release --locked --bin kiki \
    && strip target/release/kiki

# Runtime stage: minimal Debian with just the libs the binary links against.
FROM debian:trixie-slim

# ca-certificates only: rustls reads the system trust store via
# rustls-platform-verifier, but nothing links libssl any more.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        tini \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 1000 kiki \
    && useradd --system --uid 1000 --gid kiki \
        --home-dir /data --shell /usr/sbin/nologin --no-create-home kiki \
    && install -d -m 0750 -o kiki -g kiki /data

COPY --from=builder /build/target/release/kiki /usr/local/bin/kiki
COPY --chmod=0755 docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh

# kiki serve is hardcoded to look for ./kiki.db, so the working directory
# must be the volume mount.
WORKDIR /data
VOLUME ["/data"]

ENV KIKI_DATA_DIR=/data \
    KIKI_BIND=0.0.0.0 \
    KIKI_PORT=8000

EXPOSE 8000

USER kiki

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/docker-entrypoint.sh"]
CMD ["serve"]
