# syntax=docker/dockerfile:1

FROM rust:1-bookworm AS builder
WORKDIR /workspace

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY docs/examples ./docs/examples

RUN cargo build --release -p nexus-server --bin nexus-server

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home --home-dir /var/lib/domyn-nexus domyn-nexus \
    && mkdir -p /etc/domyn-nexus /var/lib/domyn-nexus /var/log/domyn-nexus /var/backups/domyn-nexus \
    && chown -R domyn-nexus:domyn-nexus /var/lib/domyn-nexus /var/log/domyn-nexus /var/backups/domyn-nexus

COPY --from=builder /workspace/target/release/nexus-server /usr/local/bin/nexus-server
COPY docs/examples/production-single-node.json /etc/domyn-nexus/config.json

USER domyn-nexus
EXPOSE 8443 7687
ENV DOMYN_NEXUS_CONFIG=/etc/domyn-nexus/config.json
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD curl -fk https://127.0.0.1:8443/health || exit 1
ENTRYPOINT ["/usr/local/bin/nexus-server"]
