# syntax=docker/dockerfile:1

# ---- build ----
FROM rust:1.99.0-bookworm AS builder
WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY src ./src
# Migrations, the schema SQL and the CA bundle are embedded at compile time
# (include_str!), so they are needed here and not in the runtime image.
COPY migrations ./migrations
COPY certs ./certs

RUN cargo build --release --locked --bin evmhot --bin evmhot-gapscan

# ---- runtime ----
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --no-create-home --shell /usr/sbin/nologin evmhot

COPY --from=builder /app/target/release/evmhot /usr/local/bin/evmhot
COPY --from=builder /app/target/release/evmhot-gapscan /usr/local/bin/evmhot-gapscan

USER 10001
# chains.toml is mounted or baked into a derived image; see CHAINS_CONFIG.
ENV PORT=3000 \
    CHAINS_CONFIG=/etc/evmhot/chains.toml
EXPOSE 3000
ENTRYPOINT ["/usr/local/bin/evmhot"]
