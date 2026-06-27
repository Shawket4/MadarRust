# ── Stage 1: generate cargo-chef recipe ──────────────────────────────────────
FROM rust:1.88-slim AS planner
WORKDIR /app
RUN cargo install cargo-chef --locked
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ── Stage 2: compile dependencies (cached layer) ──────────────────────────────
FROM rust:1.88-slim AS builder
WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev curl \
    && rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-chef --locked
COPY --from=planner /app/recipe.json recipe.json
# This layer is only invalidated when Cargo.lock changes
RUN cargo chef cook --release --recipe-path recipe.json

COPY . .
# LTO_MODE=fat (default, CI/prod) or thin (dev — halves link memory)
ARG LTO_MODE=fat
RUN CARGO_PROFILE_RELEASE_LTO=${LTO_MODE} cargo build --release --bin madar-rust

# ── Stage 3: minimal runtime ──────────────────────────────────────────────────
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/madar-rust /usr/local/bin/madar-rust
WORKDIR /app
EXPOSE 8081
CMD ["madar-rust"]
