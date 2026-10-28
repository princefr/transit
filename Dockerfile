# Multi-stage release build for the transit GraphQL server.
FROM rust:1-bookworm AS builder
WORKDIR /app

# ── Dependency cache layer ───────────────────────────────────────────────────
# Copy only manifests + build inputs, compile against stub sources so the
# (slow) third-party dependency build is cached until Cargo.toml/lock change.
# Workspace members and path dependencies (crates/ban-search) must be present
# for cargo to resolve the workspace at all.
COPY Cargo.toml Cargo.lock build.rs ./
COPY crates/ban-search/Cargo.toml ./crates/ban-search/Cargo.toml
COPY proto ./proto
RUN mkdir -p src crates/ban-search/src/bin \
    && printf 'pub fn _stub() {}\n' > src/lib.rs \
    && printf 'fn main() {}\n' > src/main.rs \
    && printf 'pub fn _stub() {}\n' > crates/ban-search/src/lib.rs \
    && printf 'fn main() {}\n' > crates/ban-search/src/bin/ban_index.rs \
    && cargo build --release --locked --bin transit \
    && rm -rf src crates/ban-search/src

# ── Real build ───────────────────────────────────────────────────────────────
COPY crates ./crates
COPY src ./src
COPY config ./config
# Ensure sources are newer than the stub compile so cargo rebuilds them.
# A failing build must fail the image build: no `|| true` here.
RUN find src crates -name '*.rs' -exec touch {} + \
    && cargo build --release --locked --bin transit
# Stripping is best-effort only.
RUN strip /app/target/release/transit || true

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
# config/default.toml binds to loopback; inside a container we must listen on
# all interfaces (publish the port on the host's loopback — see docker-compose.yml).
ENV TRANSIT__SERVER__BIND=0.0.0.0:8080
ENV RUST_LOG=info,transit=info
EXPOSE 8080
VOLUME ["/app/data"]

HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
  CMD curl -fsS http://127.0.0.1:8080/health || exit 1

CMD ["transit"]
