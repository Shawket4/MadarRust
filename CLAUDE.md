# Claude Code Conventions — MadarRust (backend)

## Project Overview
`madar-rust` is the **single source of truth** for the Madar ecosystem: the HTTP API,
the database schema, and the money/cost engine. Actix-Web over SQLx/PostgreSQL.

Every other project in the ecosystem is a *consumer* of this one — the OpenAPI spec
this repo exports is the contract they generate their clients from. A change here is
usually a change in two or three repos.

## The Madar ecosystem — three repos

| Repo | Path | Role |
|---|---|---|
| **MadarRust** (this) | `/Users/magd/MadarRust` | Actix-Web API, Postgres schema, money/cost engine |
| **MadarDashboard** | `/Users/magd/MadarDashboard` | React 19 management dashboard (+ Tauri desktop, public ordering, landing) |
| **madar** | `/Users/magd/madar` | Flutter POS/teller + KDS, over a shared Rust core (`rust-core/`) |

`openapi.json` in this repo is the contract. Regenerate it here, then regenerate
the clients downstream:

```
cargo run --bin export-openapi                 # this repo → openapi.json
cd /Users/magd/MadarDashboard && npm run generate:api   # → src/data/api/generated (orval)
cd /Users/magd/madar/rust-core && ./tool/generate_api.sh  # → crates/madar-api (reads ../../MadarRust)
```

**Adding or changing an endpoint is not done until the consumers are regenerated.**
The Flutter side additionally needs `melos run bridge` when the FRB surface changes.

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
- **Test**: `DATABASE_URL=postgres://shawket@localhost:5433/madar cargo nextest run` (see "Running the test suite fast" below; plain `cargo test` still works but is slower and a hung test hangs the whole run)
- **Check**: `cargo check`
- **OpenAPI Export**: `cargo run --bin export-openapi`
- **Reprice order cost snapshots at current recipes & ingredient costs** (operator-only, never exposed over HTTP):
  `cargo run --bin backfill-cost-snapshots -- (--org <uuid> | --branch <uuid>) [--dry-run]`
  Rewrites `order_items.unit_cost/line_cost` + addon/optional/bundle-component costs as if each
  line were ordered today (current recipe/addon rollups × quantities — mirrors the menu-engineering
  `cost_basis=current` view). Always `--dry-run` first.

## Deploy: required environment
The server checks its required settings BEFORE connecting or running migrations (`src/boot_config.rs`) and exits with every problem listed, so a bad `.env` never migrates prod and then crash-loops:
- `DATABASE_URL`, `JWT_SECRET` — always.
- `ASSET_URL_SECRET` — **release builds** (>= 32 bytes, `openssl rand -hex 32`; a fresh value per environment). It keys the signed org-scoped asset URLs; debug builds fall back to a dev key.
- `SSL_CERT_FILE` / `SSL_KEY_FILE` — optional, but once both are set they must be readable.
See `.env.example` (prod), `deploy/demo/.env.example` (demo) and `docker-compose.loadtest.yml` (load rig).

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

### Running the test suite fast
The Rust code is not what's slow: each `#[sqlx::test]` creates a fresh database and replays the full migration set, then drops it. Postgres disk syncing and migration replay dominate. So:
- **Use the dedicated throwaway test cluster on port 5433**, never the real `:5432` server (which holds `madar_dev`, `madar_prodcopy` and other real data). It runs with `fsync=off`, `synchronous_commit=off`, `full_page_writes=off`, `autovacuum=off`, `max_connections=500` — safe only because nothing on it matters.
  - Data dir `~/.madar-test-pg`; start: `/opt/homebrew/opt/postgresql@17/bin/pg_ctl -D ~/.madar-test-pg -l ~/.madar-test-pg/server.log start`.
  - Bootstrap once: `initdb -D ~/.madar-test-pg -U shawket --auth=trust`, append the settings above + `port = 5433` to its `postgresql.conf`, then `psql -p 5433 -d postgres -c "CREATE ROLE sufrix NOLOGIN" -c "CREATE ROLE madar_app NOLOGIN"`, `createdb -p 5433 madar`, `DATABASE_URL=postgres://shawket@localhost:5433/madar sqlx migrate run` (the compile-time `sqlx::query!` checks need a migrated `madar` DB there; re-run `sqlx migrate run` after adding a migration).
- **Use `cargo nextest run`** (`brew install cargo-nextest`; config in `.config/nextest.toml`): real per-test parallelism, and a test slower than 60s is flagged and killed at 3 min instead of silently hanging the run. Filter while iterating: `cargo nextest run --lib -E 'test(tills::)'`.
- **Pre-migrated template (template DB + migration baseline in one):** on the 5433 cluster `template1` is itself migrated (`DATABASE_URL=postgres://shawket@localhost:5433/template1 sqlx migrate run`). `#[sqlx::test]`'s `CREATE DATABASE` copies `template1`, so every per-test DB is born with the full schema and `_sqlx_migrations` already filled — sqlx sees all checksums match and replays nothing. No test code changes, no squashed migration file to keep in sync. **After adding or editing a migration, re-run `sqlx migrate run` on both `template1` and `madar` on 5433**; a checksum mismatch error in tests means the template is stale. Never do this on `:5432`.
- **Never pass `--no-capture` to nextest for a multi-test run**: it forces tests to run one at a time and looks exactly like a hang. Failing tests print their output anyway. Don't pipe a run through `sort`/`uniq`, which hides all progress until the end.
- **Never run two full suites at once** against the same cluster, and never pass `--test-threads` below the default without a reason.
- **A run that sits at ~0% CPU with test connections idle (`ClientRead`) is a deadlock in app code** (usually a handler holding a transaction while waiting for a second pool connection, or an advisory lock), not slowness. nextest's timeout names the test; fix the cause.
- Test databases are named `_sqlx_test_*`; after killed runs drop leftovers: `psql -p 5433 -d postgres -Atc "select 'drop database \"'||datname||'\";' from pg_database where datname like '_sqlx_test%'" | psql -p 5433 -d postgres`.

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
- `src/main.rs` / `src/lib.rs` — entry point and module exports; `src/bin/` holds the
  operator binaries (`export-openapi`, `backfill-cost-snapshots`, `fuzz-token`).
- `src/errors.rs` — `AppError` + `status_for_sqlstate`. Lean on it and new handlers
  inherit correct DB-error → HTTP mapping.
- `migrations/` — `sqlx` migrations, applied on boot. **Never edit an applied
  migration** (checksums; live DBs fail to start).
- **A table with `deleted_at` expresses uniqueness as a PARTIAL index**
  (`... WHERE deleted_at IS NULL`), never a plain `UNIQUE`. A plain one keeps a
  deleted row's name reserved for ever, so deleting something and creating it
  again — the most ordinary correction there is — fails with a conflict against
  a row the person cannot see. Five tables had this wrong and three had it
  right; assume the next one will get it wrong unless someone checks.
- `tests/`, `api_dumps/`, `scripts/`, `loadtest/`.

### Module map (`src/<module>/{mod,handlers,routes,tests}.rs`)
Each feature module owns its routes, handlers and tests together.

- **Identity & access** — `auth`, `users`, `orgs`, `branches`, `permissions`
  (role × resource × action, seeded by `permissions::seeder`).
- **Selling** — `orders`, `tickets` (waiter open tickets), `held_orders` (POS parked
  carts + table occupancy + transfer waitlist), `tills`, `shifts`, `payment_methods`,
  `discounts`, `bundles`.
- **Catalog & cost** — `menu`, `menu_unification`, `recipes`, `costing`, `units`,
  `inventory`, `purchasing`, `stocktakes`.
- **Floor** — `reservations` (`floor.rs` = sections + table geometry + live status;
  `bookings.rs`/`public.rs` = the DEPRECATED booking flow).
- **Fulfilment** — `delivery`, `geo`, `kitchen`, `qr_card`.
- **Platform** — `realtime` (per-branch SSE hub), `sync` (offline replay), `reports`,
  `insights`, `ai`, `integrations`, `uploads`, `translation`, `rate_limit`, `cache`.

## Cross-cutting contracts worth knowing

### Offline replay
The POS is offline-first. Every mutating POS operation is split **live route** /
`*_inner` core so `/sync/replay` can flush a till's queued backlog through exactly the
same code path (see `src/sync/handlers.rs` and the `*_inner` fns in `tickets`,
`held_orders`). If you add a POS-facing mutation, split it the same way or offline
tills silently lose the write.

### Realtime
`src/realtime` publishes per-branch events consumers subscribe to, e.g.
`floor.layout_changed` (re-pull the authored layout) and `table.status_changed`.
Publish **after** `tx.commit()`, never inside the transaction.

### The floor / tables feature (spans all three repos)
- `branch_tables` is one entity shared by three features: QR targets, floor geometry,
  and live occupancy. It is also the **per-table mutex** — every occupancy mutation
  locks its row (`SELECT … FOR UPDATE`) and then checks both held orders and open
  tickets in the same transaction. Invariant: at most one live occupant per table.
- Permissions are split on purpose: `floor_plan` = geometry authoring (managers,
  dashboard), `reservations` = live table status (host/teller, POS).
- **Bussing:** a checkout does NOT free its table. `complete` (held order) and settle
  (open ticket) call `bus_table` → status `dirty`; the table stays there until a human
  clears it on the POS. Moves/voids/discards call `free_table` → `free`, because no
  party vacated. Both live in `src/held_orders/mod.rs`.
- The booking flow (`/reservations`, `/public/reservations`) is **deprecated** and only
  mounts behind `MADAR_ENABLE_RESERVATIONS`; `/floor/*` is always mounted.

### Inventory (v2, count-first — see `INVENTORY_V2.md`)
- The org catalog is the only setup; `branch_stock` rows are created lazily and are
  never a precondition (every read is `org_ingredients LEFT JOIN branch_stock`).
- `branch_stock.on_hand` is **derived from the ledger**: post an
  `inventory_movements` row via `inventory::movements::record_movement` and read the
  balance back. A DB guard trigger rejects any direct `UPDATE … SET on_hand`; the one
  sanctioned exception is the unit re-denomination in `update_catalog_item`
  (`SET LOCAL madar.stock_rebase = 'on'`).
- Stock counts measure variance against live book stock at finalize, never the
  opening snapshot. Ingredient categories are a table keyed by `slug`; `milk` and
  `coffee_bean` slugs drive the menu swap logic.

## Related Projects (Ecosystem)
- **MadarDashboard**: `/Users/magd/MadarDashboard` — management dashboard. Consumes this
  API via orval-generated hooks; also ships the public ordering + landing bundles.
- **madar**: `/Users/magd/madar` — Flutter POS/teller, KDS, and the shared Rust core
  (`rust-core/crates/madar-core`) that mirrors this API **offline** and replays queued
  writes through `/sync/replay`.

Both consume `openapi.json`; see the codegen commands at the top of this file. When you
change a handler's request/response shape, check whether the POS's offline core
(`rust-core/crates/madar-core`) mirrors that shape too — it keeps its own lenient copies
of the wire types so old app builds keep parsing new payloads.
