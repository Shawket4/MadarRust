# Claude Code Conventions

## Project Overview
This is a Rust backend project (`madar-rust`) built for the Madar ecosystem. It uses Actix-Web for the HTTP framework and SQLx (PostgreSQL) for database interactions.

## Technology Stack
- **Language**: Rust (Edition 2024)
- **Web Framework**: Actix-Web 4
- **Database**: PostgreSQL (via `sqlx` 0.7)
- **Async Runtime**: Tokio
- **API Documentation**: OpenAPI / Swagger (via `utoipa` 5.5)
- **Serialization**: Serde, JSON

## Development Workflow
- **Build**: `cargo build`
- **Run**: `cargo run`
- **Test**: `cargo test`
- **Check**: `cargo check`
- **OpenAPI Export**: `cargo run --bin export-openapi`
- **Reprice order cost snapshots at current recipes & ingredient costs** (operator-only, never exposed over HTTP):
  `cargo run --bin backfill-cost-snapshots -- (--org <uuid> | --branch <uuid>) [--dry-run]`
  Rewrites `order_items.unit_cost/line_cost` + addon/optional/bundle-component costs as if each
  line were ordered today (current recipe/addon rollups × quantities — mirrors the menu-engineering
  `cost_basis=current` view). Always `--dry-run` first.

## Robustness & Pre-push Testing
Deterministic test tooling guards the codebase (especially the money/cost engine). Run it locally **before pushing** with the tiered gate:

- **`scripts/preflight.sh`** — local pre-push CI.
  - Default (no flags) = FAST gate: `cargo fmt --check` + `cargo clippy` + `cargo test --lib`.
  - Opt-in heavier stages: `--mutants` (cargo-mutants on the lines you changed, `--in-diff`), `--full-mutants` (full money-engine sweep, ~15 min), `--fuzz` (cargo-fuzz smoke), `--schemathesis` (API fuzz, gates on any 5xx), `--restler` (stateful fuzz; needs the x86_64 VM), `--all`.
  - Env: `DATABASE_URL` (default dev DB on `:5432`), `STRICT=1` to make fmt/clippy block too. Exit code is non-zero if any gate fails.
  - Install as a git hook: `ln -sf ../../scripts/preflight.sh .git/hooks/pre-push`

The tools (and where they live):
- **cargo-mutants** (`.cargo/mutants.toml`) — mutation testing; `--in-diff` follows changed lines, so it stays fast and adaptive. `cargo install cargo-mutants cargo-nextest`.
- **cargo-fuzz** (`fuzz/`, nightly) — coverage-guided fuzzing of the pure money/geo/discount fns (`round_piastres`, `calc_discount`, `convert`/`convert_with_density`, `blend_weighted_cost`, `summarize_line_costs`, `select_zone_fee`, `haversine_meters`). `cargo +nightly fuzz run <target>`.
- **Schemathesis** (`scripts/api-fuzz.sh`, `scripts/seed_fuzz.sql`, `src/bin/fuzz-token`) — schema-driven API fuzzing of every endpoint against a **disposable `madar_fuzz` DB**; checks for 5xx + schema/contract conformance. Re-exports the spec each run so it always matches the current API.
- **RESTler** (`scripts/restler-run.sh`, `scripts/openapi_31_to_30.py`) — stateful API fuzzing. RESTler's amd64 .NET **segfaults under Rosetta**, so it needs a real x86_64 VM: `colima start --arch x86_64 --vm-type qemu` (after `brew install qemu lima-additional-guestagents`). RESTler can't parse OpenAPI 3.1, so the spec is downconverted to 3.0 first.
- **k6 load testing** (`scripts/loadtest.sh`, `docker-compose.loadtest.yml`, `loadtest/`) — drives the real release binary in Docker, **resource-capped to mimic the prod VPS** (1 vCPU / 4 GB, Postgres co-resident: both containers pinned to `cpuset: "0"`), with [k6](https://k6.io) on the host. Profiles `smoke|ramp|soak|spike|pos-day|all`; `scripts/loadtest.sh ramp`. Authenticated org-admin read/write mix (writes hit the money engine). Caveat: Apple-silicon cores are faster than a Hostinger vCPU, so absolute latency is optimistic — throttle with `BACKEND_CPUS=0.5`. See `loadtest/README.md`.
- **CI** (`.github/workflows/ci.yml`) — PR gate (test + clippy + fmt with a Postgres service) + nightly mutants/fuzz; mirrors `preflight.sh`.

Notes:
- Tests + mutants need Postgres and `DATABASE_URL` set **at build time** (the suite uses the `sqlx::query!` compile-time macro and `#[sqlx::test]` per-test DBs): `DATABASE_URL=postgres://shawket@localhost:5432/madar cargo test --lib` (local dev DB is `madar`, owned by superuser `shawket`).
- Legacy `sufrix` role: several pre-rebrand migrations (`GRANT ALL ... TO sufrix`) target a role from the Sufrix era. Each `#[sqlx::test]` rebuilds a DB from the full migration set, so that **cluster-global** role must exist or every DB test aborts with SQLSTATE 42704 (`role "sufrix" does not exist`). `preflight.sh` / `api-fuzz.sh` / CI auto-create it (idempotent `CREATE ROLE sufrix NOLOGIN`); to do it by hand once: `psql -c "CREATE ROLE sufrix NOLOGIN"`. Don't edit those applied migrations to rename the role — it changes their checksums and the live dev/prod DBs (which re-run `sqlx::migrate!` on boot) would fail to start.
- Fuzz/API-fuzz runs set `MADAR_DISABLE_AUTO_TRANSLATION=1` (no outbound Google Translate) and `MADAR_DISABLE_RATE_LIMIT=1` (no 429 throttling). **Never set these in production.**
- DB-error → HTTP mapping is centralized in `src/errors.rs` (`status_for_sqlstate`): client-caused SQLSTATEs become 4xx, not 500. Keep new handlers leaning on `AppError` so they inherit this.

## Coding Guidelines
1. **Idiomatic Rust**: Follow standard Rust formatting (`cargo fmt`) and linting (`cargo clippy`).
2. **Error Handling**: Use `thiserror` for defining custom domain errors. Avoid unwrapping unless absolutely necessary (e.g., in tests).
3. **Database**: Use `sqlx` macros for compile-time checked queries. Ensure migrations are placed in the `migrations/` directory.
4. **API Documentation**: All new endpoints must be annotated with `#[utoipa::path(...)]` and included in the OpenAPI documentation. Make sure to define response schemas using `ToSchema` for structures.
5. **Types**: Use `uuid::Uuid` for primary keys and references. Use `rust_decimal::Decimal` or `bigdecimal::BigDecimal` for currency and financial calculations to avoid floating-point errors.
6. **Async**: Rely on `tokio` for async operations. Use `actix_web::web::Data` for shared application state (e.g., database connection pools).

## File Structure
- `src/`: Main application source code.
  - `main.rs`: Entry point for the server.
  - `lib.rs`: Library module exports.
  - `bin/`: Additional binaries (e.g., `export_openapi.rs`).
- `tests/`: Integration tests.
- `migrations/`: SQL files for `sqlx` migrations.
- `api_dumps/`: Stored OpenAPI and API dump data.

## Related Projects (Ecosystem)
When working on API changes or feature implementations, be aware that this backend interacts with other projects in the Madar ecosystem. You may need to search or modify code in these directories:
- **MadarDashboard**: `/Users/shawket/Desktop/MadarDashboard` (Frontend Dashboard application)
- **Madar POS**: `/Users/shawket/Desktop/madar_pos` (Point of Sale frontend application)
You can use file read commands or `cd` into these directories to analyze frontend consumption of this backend API.
