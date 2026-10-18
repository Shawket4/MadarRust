# madar-shared (git dependency) is PRIVATE. Each cargo RUN below mounts the
# BuildKit secret `madar_shared_token` (docker build --secret
# id=madar_shared_token,env=...) and, only inside that RUN, points git at it via
# GIT_CONFIG_* env (nothing is written to disk, so no layer or cache entry holds
# the token). Secret absent or empty: the RUN is the same anonymous build as before.
# ── Stage 1: generate cargo-chef recipe ──────────────────────────────────────
FROM rust:1.88-slim AS planner
WORKDIR /app
RUN cargo install cargo-chef --locked
COPY . .
RUN --mount=type=secret,id=madar_shared_token \
    set -e; \
    if [ -s /run/secrets/madar_shared_token ]; then \
      apt-get update && apt-get install -y --no-install-recommends git && rm -rf /var/lib/apt/lists/*; \
      export CARGO_NET_GIT_FETCH_WITH_CLI=true GIT_CONFIG_COUNT=1 \
        GIT_CONFIG_KEY_0="url.https://x-access-token:$(cat /run/secrets/madar_shared_token)@github.com/Shawket4/madar-shared.insteadOf" \
        GIT_CONFIG_VALUE_0="https://github.com/Shawket4/madar-shared"; \
    fi; \
    cargo chef prepare --recipe-path recipe.json

# ── Stage 2: compile dependencies (cached layer) ──────────────────────────────
FROM rust:1.88-slim AS builder
WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev curl \
    && rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-chef --locked
COPY --from=planner /app/recipe.json recipe.json
# madar-authz (and the other madar-shared crates) are GIT dependencies pinned
# by tag; cargo-chef carries them in the recipe and `cook` fetches them, so no
# stub is needed. This layer is only invalidated when Cargo.lock changes.
#
# LTO_MODE=fat (default, CI/prod) or thin (dev — halves link memory). It is set
# for `cook` AND for the build below, on purpose: Cargo fingerprints the profile,
# so dependencies cooked under one LTO value are compiled again under another
# (a deploy compiled all of them twice, ~4 min, until this was the same on both).
ARG LTO_MODE=fat
RUN --mount=type=secret,id=madar_shared_token \
    set -e; \
    if [ -s /run/secrets/madar_shared_token ]; then \
      apt-get update && apt-get install -y --no-install-recommends git && rm -rf /var/lib/apt/lists/*; \
      export CARGO_NET_GIT_FETCH_WITH_CLI=true GIT_CONFIG_COUNT=1 \
        GIT_CONFIG_KEY_0="url.https://x-access-token:$(cat /run/secrets/madar_shared_token)@github.com/Shawket4/madar-shared.insteadOf" \
        GIT_CONFIG_VALUE_0="https://github.com/Shawket4/madar-shared"; \
    fi; \
    CARGO_PROFILE_RELEASE_LTO=${LTO_MODE} cargo chef cook --release --recipe-path recipe.json

COPY . .
RUN --mount=type=secret,id=madar_shared_token \
    set -e; \
    if [ -s /run/secrets/madar_shared_token ]; then \
      apt-get update && apt-get install -y --no-install-recommends git && rm -rf /var/lib/apt/lists/*; \
      export CARGO_NET_GIT_FETCH_WITH_CLI=true GIT_CONFIG_COUNT=1 \
        GIT_CONFIG_KEY_0="url.https://x-access-token:$(cat /run/secrets/madar_shared_token)@github.com/Shawket4/madar-shared.insteadOf" \
        GIT_CONFIG_VALUE_0="https://github.com/Shawket4/madar-shared"; \
    fi; \
    CARGO_PROFILE_RELEASE_LTO=${LTO_MODE} cargo build --release --bin madar-rust

# ── Stage 3: minimal runtime ──────────────────────────────────────────────────
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    `# libssl3: Apple Wallet pass signing (PKCS#7 over the manifest).` \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/madar-rust /usr/local/bin/madar-rust
WORKDIR /app
# The step-animation library ships WITH the binary rather than in a volume: it
# is curated in git, so the running image and the repo can never disagree, and
# adding one is a commit plus the deploy that already happens.
COPY static ./static
EXPOSE 8081
CMD ["madar-rust"]
