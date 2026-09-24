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
- **Test**: `scripts/run_tests.sh` (everything) or `scripts/run_tests.sh orders menu` (named suites). Use it rather than a bare `cargo nextest run` — see "Where the tests live" and "Running the test suite fast" below for the two reasons why.
- **Check**: `cargo check`
- **OpenAPI Export**: `cargo run --bin export-openapi`
- **Reprice order cost snapshots at current recipes & ingredient costs** (operator-only, never exposed over HTTP):
  `cargo run --bin backfill-cost-snapshots -- (--org <uuid> | --branch <uuid>) [--dry-run]`
  Rewrites `order_items.unit_cost/line_cost` + addon/optional/bundle-component costs as if each
  line were ordered today (current recipe/addon rollups × quantities — mirrors the menu-engineering
  `cost_basis=current` view). Always `--dry-run` first.

- **What the customers-unification migrations will do to a database, WITHOUT writing** (run against a COPY of prod before deploying; applies the pending migrations in one transaction, reports created/merged/conflicts, rolls back — see `docs/customers-unification-deploy.md`):
  `DATABASE_URL=<copy> cargo run --bin customers-backfill-dry-run`

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

### Where the tests live

**Every test is an integration test, under `tests/`, one binary per suite.** `src`
holds no `mod tests` files at all; only small inline `#[cfg(test)] mod tests`
blocks beside pure functions remain, and those run under `--lib`.

This is not tidiness. The single lib test binary reached ~295 MB and **XProtect
returns a malware verdict on it**: it stalls in `_dyld_start`, never reaches
`main`, is killed with SIGKILL, and is then deleted from disk. To cargo that
looks like a corrupt or vanished artifact, and it cost days before anyone read it
as a security action. A binary of ~70–100 MB is not matched; splitting is what
makes the suite runnable at all. See `XPROTECT_FALSE_POSITIVE.md` in the parent
directory.

Consequences worth knowing:

- **`#[cfg(test)]` does not reach these suites.** An integration test binary links
  the library the way production does. A `#[cfg(test)]` helper in `src` is
  invisible to them, and — worse — a `#[cfg(test)]` / `#[cfg(not(test))]` pair
  silently gives them the production branch. `src/db.rs` did exactly that with the
  tenant pool's idle timeout and cost ~5 s per test. If you need a switch for
  tests, gate it on an env var AND `debug_assertions`, never on `cfg(test)` alone.
- **Fixtures shared by two suites live in `tests/common/`**, never in a sibling
  suite. Reaching into another suite's helpers is what `tests/common/{assets,
  analytics,sizes}.rs` exists to replace.
- **A test-only hook in `src` must not be `#[cfg(test)]`.** Make it
  `#[doc(hidden)] pub` (inert, e.g. `ai::mock`, `qr_card::shlink::fake`) or gate
  it on `debug_assertions` when it changes behaviour (e.g. `assets::ingest`'s
  `FAIL_BEFORE_COMMIT` fault injection, which release builds compile out).
- **`tests/client_seen.rs` is skipped by the runner.** Its binary is killed on
  exec: its fixtures embed complete browser User-Agent strings, which is the most
  likely signature match. Rewriting test data so a malware scanner stops matching
  it is an evasion decision for the owner, not a refactor — so it is named and
  skipped, never silently dropped.

### Running the test suite fast
Measured 2026-09-20, and most of the old folklore here was wrong. `CREATE
DATABASE` from the migrated template is **90 ms**; the whole harness floor (an
empty `#[sqlx::test]`) is **0.21 s**; all the fixture seeding an orders test does
is **0.19 s**. None of those were the cost.

Two things actually dominated, both now fixed:

- **The tenant pool's production idle timeout** reaching the suites through a
  `cfg(test)` switch that no longer applies (see above). ~5 s per test that
  serves a request, because the throwaway database cannot be dropped while a
  tenant connection lingers. `MADAR_FAST_TEST_POOLS=1` (set by
  `scripts/run_tests.sh`) restores the short reaping.
- **Running the whole suite in one `cargo nextest run`.** ~1660 tests create
  enough per-test databases to fill the 12 GiB RAM cluster, and whichever suite is
  running when it fills dies with `PoolTimedOut` — which reads as broken code. The
  cluster cannot grow: the machine has 24 GiB total. `scripts/run_tests.sh` runs
  one suite at a time and sweeps `_sqlx_test%` between them, holding at ~2 GiB.

Everything below still applies:
- **The :5433 cluster now lives on a RAM disk** (`/Volumes/MadarTestRAM/pg`, owner-approved 2026-09-17): per-test `CREATE DATABASE` copies `template1` in memory instead of on the SSD. Connect exactly as before. It is volatile: recreate it after a reboot with `../tool/test_pg_ram.sh`, and see `../tool/test_pg.md`. Agents use THIS cluster rather than starting their own; a private cluster (for a branch with a different migration set) also goes on the RAM disk, on its own port.
- **Use the dedicated throwaway test cluster on port 5433**, never the real `:5432` server (which holds `madar_dev`, `madar_prodcopy` and other real data). It runs with `fsync=off`, `synchronous_commit=off`, `full_page_writes=off`, `autovacuum=off`, `max_connections=500` — safe only because nothing on it matters.
  - Data dir `~/.madar-test-pg`; start: `/opt/homebrew/opt/postgresql@17/bin/pg_ctl -D ~/.madar-test-pg -l ~/.madar-test-pg/server.log start`.
  - Bootstrap once: `initdb -D ~/.madar-test-pg -U shawket --auth=trust`, append the settings above + `port = 5433` to its `postgresql.conf`, then `psql -p 5433 -d postgres -c "CREATE ROLE sufrix NOLOGIN" -c "CREATE ROLE madar_app NOLOGIN"`, `createdb -p 5433 madar`, `DATABASE_URL=postgres://shawket@localhost:5433/madar sqlx migrate run` (the compile-time `sqlx::query!` checks need a migrated `madar` DB there; re-run `sqlx migrate run` after adding a migration).
- **Use `cargo nextest run`** (`brew install cargo-nextest`; config in `.config/nextest.toml`): real per-test parallelism, and a test slower than 60s is flagged and killed at 3 min instead of silently hanging the run. Filter while iterating: `cargo nextest run --lib -E 'test(tills::)'`.
- **Pre-migrated template (template DB + migration baseline in one):** on the 5433 cluster `template1` is itself migrated (`DATABASE_URL=postgres://shawket@localhost:5433/template1 sqlx migrate run`). `#[sqlx::test]`'s `CREATE DATABASE` copies `template1`, so every per-test DB is born with the full schema and `_sqlx_migrations` already filled — sqlx sees all checksums match and replays nothing. No test code changes, no squashed migration file to keep in sync. **After adding or editing a migration, re-run `sqlx migrate run` on both `template1` and `madar` on 5433**; a checksum mismatch error in tests means the template is stale. Never do this on `:5432`.
- **Never pass `--no-capture` to nextest for a multi-test run**: it forces tests to run one at a time and looks exactly like a hang. Failing tests print their output anyway. Don't pipe a run through `sort`/`uniq`, which hides all progress until the end.
- **Never run two full suites at once** against the same cluster, and never pass `--test-threads` below the default without a reason.
- **A run that sits at ~0% CPU with test connections idle (`ClientRead`) is a deadlock in app code** (usually a handler holding a transaction while waiting for a second pool connection, or an advisory lock), not slowness. nextest's timeout names the test; fix the cause.
- **While other agents build, run the suite from a nextest ARCHIVE.** Worktrees that share one `CARGO_TARGET_DIR` overwrite each other's *unhashed* binaries (`target/debug/<name>` is a hardlink to `deps/<name>-<hash>`, and last writer wins). A plain `cargo nextest run` then dies mid-run with `failed to exec .../deps/madar_rust-<hash>: No such file or directory` — deterministically, hundreds of tests in, looking like a mass test failure rather than a build problem. Build once, then run from the archive, which extracts to a private directory nothing else touches:
  ```
  cargo nextest archive --archive-file /tmp/madar_tests.tar.zst      # ~160 MB, needs DATABASE_URL
  cargo nextest run --archive-file /tmp/madar_tests.tar.zst --workspace-remap .
  ```
  The same collision silently breaks `cargo run --bin export-openapi`: it can run ANOTHER worktree's binary and write a spec with none of your work in it. Verify before trusting it (`strings target/debug/export-openapi | grep <your-new-route>`), or run the hashed binary in `target/debug/deps/` directly.
- **A shared `CARGO_TARGET_DIR` could once give you ANOTHER worktree's `madar-authz`** (a path dependency, fingerprinted by relative path). It is a git dependency pinned by tag now, so that trap is gone; the same trap applies to any path dependency you add.
- **A `mis-aligned LINKEDIT string pool` dlopen error on a proc-macro dylib means it was STRIPPED, not that the crate or the disk is broken.** Apple's `strip` on Xcode 27 / macOS 27 corrupts any dylib it touches (the same family as the POS `libmadar_frb` bug), and cargo strips automatically when a profile turns debug info off. The trap is `[profile.dev.package."*"] debug = false` in a machine-local `.cargo/config.toml`, added for build speed: it silently enables stripping, and then `libsqlx_macros` — or any proc-macro — links to a dylib nothing can `dlopen`, so the whole workspace stops compiling. Set `strip = false` explicitly in every profile block that lowers `debug`. Symptoms that mislead: the corruption is DETERMINISTIC (same hash, same size, same byte offset every rebuild) and survives `cargo clean -p`, re-signing with `codesign`, and freeing disk, because each rebuild strips again.
- **Disk:** `target/debug/deps` accumulates `*.rcgu.o` codegen intermediates from interrupted builds and cargo never collects them — tens of GB. With no cargo running, `find target/debug/deps -maxdepth 1 -name '*.rcgu.o' -delete` is safe and rustc regenerates what it needs. `target/debug/incremental` is likewise pure scratch.
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

### The POS changefeed (`/sync/pull`, offline plan B)
Every table a POS shows reaches devices through `sync_changes` (`src/sync/pull`).
- **A new POS-visible table needs a sync trigger**: an `AFTER INSERT OR UPDATE
  OR DELETE` trigger named `sync_emit` running `sync_emit_<table>()`, a row in
  `sync_source_tables()`, and a line in the migration's `SOURCE TABLES` header.
  `tills_migration_tests::every_projection_source_table_has_emitter` fails
  otherwise. A projection change for a type is enough when the table already
  re-emits that type.
- **A report formula change regenerates the shared vectors.** Changing
  `compute_system_cash`, `report_figures` or the close-method figures means
  `MADAR_WRITE_TILL_VECTORS=1 cargo nextest run --test tills_report_vectors_tests`,
  which writes `till_report_vectors.json` / `till_edge_vectors.json` into the
  madar-shared checkout beside this one (`crates/madar-till/vectors/`); the fold
  there (`madar_till::report`, the POS core's too) must agree, and the change
  ships with a madar-shared tag.
- Additive fields only on payloads the POS mirrors (old tablets decode them).

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

### Dawam: employees and the staff token (Phase A, `dawam-fix/PHASE_A_DESIGN.md`)
- **An employee is its own entity** (`employees`), optionally linked to a user
  (`employees.user_id`). Every HR/Dawam subject column is `employee_id`; actor
  columns (`created_by`, `decided_by`, …) stay users. Never key a staff record
  by a user id, and never assume `employee.id == user.id` (only rows migrated
  from `staff_profiles` share it). Creating an employee never creates a user.
- **The staff app's session is a staff token** (`staff::principal`): subject =
  employee, device-bound, 60 minutes, refreshed through `POST /auth/staff/refresh`
  with `X-Staff-Device`. Only `/staff/*` (behind `StaffAuth`) accepts it; the
  `Claims` verifier refuses it everywhere else. `/staff/me/*` handlers take the
  `Me` extractor; management handlers take `principal::caller(&req)` (a linked
  manager's phone acts through their account).
- **Branch scope goes through `staff::access`**: `gate` (held somewhere, before
  any lookup), `require_for` (at one of the employee's branches), `require_at`,
  `require_everywhere` (org-wide acts: payroll run, rules, holidays, roster
  settings), `scope` + `in_scope` for lists. No `check_permission` on `/staff`.
- A new Dawam table needs `GRANT SELECT, INSERT, UPDATE, DELETE … TO madar_app`
  in its migration and RLS; `tests/dawam_migration.rs` fails otherwise.

### Permissions (architecture E — PERMISSIONS_ARCHITECTURE.md)
- **One registry.** Every permission is a capability in madar-shared's
  `authz/spec/capabilities.toml` (github.com/Shawket4/madar-shared; stable id, key, legacy
  cell, group, tier, risk, role defaults, core roles, EN/AR). Generate in a madar-shared
  checkout with `cargo run -p authz-gen -- --dashboard ../MadarDashboard --pos ../madar`;
  its CI runs `--check`. Never hand-edit `generated.rs` or the dashboard's
  `src/generated/capabilities.ts`. A spec change ships as a madar-shared tag, bumped here
  and in the POS together.
- **One decision library.** `madar-authz` (a git dependency on madar-shared, pinned by tag;
  the POS core pins the same tag) resolves and decides for the server AND the POS core.
  No I/O in it. Never branch on a role name in new code; ask for a capability.
- **Anti-escalation is not optional.** Any write that changes access (users, roles,
  overrides, branch assignments) goes through `permissions::guard` / `madar_authz::guard`:
  no self-edit, dominate the target, hold what you grant, owners protected, last owner kept.
- **Core capabilities** (tier `core`) are always on for their role kinds and cannot be
  removed by the UI or the server. Never add a toggle that can break the app.
- **Old tablets.** Every `resource:action` cell of `GET /auth/permissions` maps to exactly
  one capability (`legacy`); `registry_tests` fails otherwise.
- **Offline-auth data** (PIN verifiers, LAN secret) only reaches a registered device of
  the org or a till worker, scoped to the device's branch; PIN hashes never ride the feed.
- Tests that need their own migrations use a private cluster when another worktree
  shares `:5433` (a missing-version error means the shared template moved).

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
