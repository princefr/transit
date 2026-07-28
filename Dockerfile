# Multi-stage release build for the transit GraphQL server.
FROM rust:1-bookworm AS builder
WORKDIR /app

# Cache dependency compilation with stub sources
COPY Cargo.toml Cargo.lock build.rs ./
COPY proto ./proto
RUN mkdir -p src \
    && printf 'pub fn _stub() {}\n' > src/lib.rs \
    && printf 'fn main() {}\n' > src/main.rs \
    && cargo build --release --bin transit || true

COPY src ./src
COPY config ./config
# Ensure sources are newer than the stub compile
RUN touch src/lib.rs src/main.rs \
    && cargo build --release --bin transit \
    && strip /app/target/release/transit || true

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/transit /usr/local/bin/transit
COPY config ./config

RUN mkdir -p /app/data \
    && useradd --system --uid 10001 --home /app transit \
    && chown -R transit:transit /app

USER transit
ENV TRANSIT__RUNTIME__DATA_DIR=/app/data
ENV RUST_LOG=info,transit=info
EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
  CMD curl -fsS http://127.0.0.1:8080/health || exit 1

CMD ["transit"]
