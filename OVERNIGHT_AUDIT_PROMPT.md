# Overnight autonomous audit — Sufrix ecosystem (backend + POS + dashboard)

You are running **unattended for ~8 hours in auto mode** with a large usage budget. Your job is an
**exhaustive bug-and-edge-case audit, with fixes and intensive tests**, across all three Sufrix
repos. **Token cost is not a constraint — favor exhaustiveness.** You are explicitly authorized to
**orchestrate many subagents in parallel** (use the Workflow tool and/or many Agent calls) and to
loop until findings dry up.

## Mission & HARD scope
- Produce a **fully tested, bug-hardened** app: backend, Flutter POS, and React dashboard.
- **ONLY bug fixes, edge-case fixes, and tests.** Do **NOT** add features, change architecture,
  refactor for style, rename things, or redesign APIs. If something is a design choice rather than a
  defect, **document it, don't change it.**
- A "fix" is the minimal change that makes a genuine defect correct. If a fix would change an API
  shape, that's allowed only when it's fixing a real bug — and then regenerate the clients (below).
- Quality bar: every test suite stays **green**, and `analyze`/`lint`/`build` stay clean. Never
  leave a suite red. If you can't fix something safely, **revert your change and log the finding.**

## 🔴 SAFETY — non-negotiable
- **PRODUCTION DB must never be touched.** Prod is `postgres://sufrix:…@100.101.100.57:5432/sufrix`.
  The backend `.env` `DATABASE_URL` points at **prod** — so **always override `DATABASE_URL`**
  explicitly for every command. NEVER run migrations, backfills, writes, or even casual connections
  against `100.101.100.57`. Do not `sqlx migrate run` against prod. If a command would inherit `.env`,
  override the URL first.
- **Use the dev DB for everything runtime.** Dev = `postgres://sufrix:REDACTED@localhost:5433/sufrix_dev`
  (PostgreSQL 17, a real prod copy, already migrated to latest). It's disposable — safe to read/write.
  - If dev PG isn't running:
    `/opt/homebrew/opt/postgresql@17/bin/pg_ctl -D ~/sufrix_dev_pg17 -o "-p 5433" -l ~/sufrix_dev_pg17/server.log start`
- **Backend unit/integration tests** use a separate **superuser** Postgres (`#[sqlx::test]` builds
  ephemeral DBs from `./migrations`): run with
  `DATABASE_URL=postgres://postgres@localhost:5432/sufrix_local cargo test`.
- Do **not** push, tag, or touch `origin`/`main` on any repo. Commit only to a local audit branch
  (see Git workflow).

## Repos & stacks
1. **Backend** `/Users/shawket/Desktop/SufrixRust` — Rust, Actix-Web 4, SQLx 0.7, Postgres, utoipa
   OpenAPI, `rust_decimal`/`BigDecimal`. Tests: `#[sqlx::test]` per-module `tests.rs` + `src/e2e_tests.rs`.
   The working tree currently has a large **uncommitted** inventory overhaul + shift/unit fixes —
   audit the working-tree state as-is.
2. **POS** `/Users/shawket/Desktop/sufrix_pos` — Flutter, Riverpod, dio. Generated API package
   `packages/sufrix_api` (regenerate via `tool/generate_api.sh`). Tests: `flutter test`. Hand-written
   models facade over the generated package in `lib/core/models/`.
3. **Dashboard** `/Users/shawket/Desktop/SufrixDashboard` — React 19 + Vite + TS, TanStack Query,
   Zustand, Orval-generated client (`npm run generate:api`, reads `../SufrixRust/openapi.json`),
   Vitest + React Testing Library + MSW, Tailwind/Radix, i18next (en/ar, RTL).

## Environment setup (do this first)
1. Ensure dev PG is up (above). Build the backend: `cd /Users/shawket/Desktop/SufrixRust && cargo build`.
2. Run the dev backend at **:8081** for any frontend↔backend / live testing (routes serve at **ROOT**,
   no `/api` prefix — that prefix is prod's nginx):
   ```
   cd /Users/shawket/Desktop/SufrixRust && \
   DATABASE_URL='postgres://sufrix:REDACTED@localhost:5433/sufrix_dev' \
   BIND_ADDR='0.0.0.0:8081' ./target/debug/sufrix-rust
   ```
   Swagger/OpenAPI: `http://localhost:8081/api-docs/swagger-ui/`.
3. **Point BOTH frontends at the local dev backend** (`http://localhost:8081`, **NO `/api` suffix**):
   - POS: `/Users/shawket/Desktop/sufrix_pos/lib/core/config/api_config.dart` — set
     `kApiBaseUrl = 'http://localhost:8081'` (there's already a commented localhost line; the active
     line is the duckdns prod URL — swap it).
   - Dashboard: find the runtime API base (grep `duckdns`, `baseURL`, `VITE_`, `/api` under
     `src/data/api/` and `.env*`) and set it to `http://localhost:8081` (drop any `/api`).
   - ⚠️ These localhost changes are for the audit run only — keep them on the audit branch; do NOT
     ship them. Note the original prod URLs so they can be restored.

## Test / build / lint commands
- **Backend:** `DATABASE_URL=postgres://postgres@localhost:5432/sufrix_local cargo test`
  (and `cargo build` / `cargo run --bin export-openapi` if a contract changes). `cargo clippy` optional.
- **POS:** `flutter pub get`; `flutter analyze`; `flutter test`. Note: `flutter analyze` does **not**
  compile `packages/sufrix_api` internals — catch compiler-only errors with `flutter build bundle`
  (Dart CFE) and `dart analyze packages/sufrix_api/lib`. Regenerate the client only if the backend
  contract changed: `OPENAPI_SPEC=/Users/shawket/Desktop/SufrixRust/openapi.json bash tool/generate_api.sh`.
- **Dashboard:** `npm install` (if needed); `npm run lint`; `npm run test` (Vitest); `npm run build`
  (typecheck+build). Regenerate client if contract changed: `npm run generate:api`.

## What to hunt — edge-case checklist (these are the bug classes that bite this app)
For every module/flow, write **multiple test cases per endpoint/behavior** (happy path + each failure
mode) and probe these specifically:
- **Race / TOCTOU & concurrency:** check-then-write without a tx/lock; uniqueness enforced only in
  app code not the DB. (We found: a teller could hold open shifts at two branches; orders attaching
  to closed/other-teller shifts.)
- **Units / measure mismatches:** quantities in the wrong base unit; cross-ingredient swaps that
  don't convert. (We found a milk→almond-milk swap deducting 1000× because g↔kg wasn't converted.)
  Money is in **piastres** everywhere (no ×100 in backend); rounding must not lose/inflate cash.
- **NULL / unknown handling:** `cost_per_unit` NULL = *unknown, never 0*; valued reports must exclude
  unknowns and count them, never treat as free.
- **Permission & multi-tenant isolation:** org→branch scoping; a token for org/branch A must not act
  on B; teller tokens are branch-bound. Note: `org_admin` is **pre-seeded** with most perms by a
  migration, so to test a *denial* use a per-user deny override (`INSERT INTO permissions(...,granted=false)`)
  or a role that lacks it — not "no grant".
- **Partial / intermediate states:** partial PO receive, partial stock counts, voids/refunds,
  force-close, idempotent retries (idempotency keys), oversold/negative stock (allowed-but-flagged).
- **Cross-entity consistency:** recipe deduction vs snapshot vs cost rollup; bundle components; addon
  overrides/optional fields; discounts/tax/tips/split payments math.
- **Boundary inputs:** empty/whitespace, negative/zero quantities, huge values, missing optional keys,
  bad enums, unknown ids (404), duplicate creates (409).
- **POS-specific:** offline queue & sync correctness, login/session resume vs the new "block login
  while a shift is open" (409) and branch-mismatch (403) handling shown as proper error states,
  shift cash math, printing fallbacks, cart/draft state.
- **Frontend-specific:** surfacing backend 4xx as readable error states (not generic), i18n/RTL,
  lists rendered in server order (the API dropped `display_order`), money formatting (EGP↔piastres),
  branch/org scope selection.

## Orchestration blueprint (use heavily)
Run this as several **Workflow** invocations (one per phase), reading results between phases so you
stay in the loop. Scale fan-out to the 8h budget.
- **Phase 1 — Map (parallel readers):** one subagent per backend module + POS feature area + dashboard
  feature → produce a structured inventory of endpoints/flows, current test coverage, and suspicious spots.
- **Phase 2 — Test & probe (fan out per module):** each subagent writes intensive `#[sqlx::test]` /
  `flutter test` / Vitest cases mirroring the gold-standard files (below), runs them, and reports
  failures + suspected edge cases. **Pipeline**, don't barrier, so fast modules don't wait.
- **Phase 3 — Adversarially verify each finding:** before any fix, spawn an independent skeptic that
  tries to prove the "bug" is actually intended behavior or a test artifact. Only fix confirmed,
  real defects. (Money-critical — avoid "fixing" non-bugs and breaking correctness.)
- **Phase 4 — Fix + re-test:** apply the **minimal** fix, re-run the suite, keep it green. If a fix
  changes a backend contract: `cargo run --bin export-openapi`, then regen POS (`tool/generate_api.sh`)
  and dashboard (`npm run generate:api`) clients and fix any fallout.
- **Phase 5 — Integration / e2e:** backend e2e (mirror `src/e2e_tests.rs` — full lifecycle:
  setup→PO receive(WAC)→sale deduction→void→adjust→transfer→stocktake guardrail→reports). Frontend↔
  backend smoke against the running localhost:8081 dev server where feasible.
- **Loop-until-dry:** repeat Phases 2–4 per module until **two consecutive rounds** surface nothing new.
- If parallel agents mutate the **same** repo's files, use `isolation: "worktree"` or serialize the
  fix step per repo to avoid conflicts.

## Fix discipline
- Minimal, bug-only. Match surrounding code style. No new deps unless strictly required by a fix.
- Re-run the affected suite after every fix; revert anything that can't be made green safely and log it.
- Preserve contracts unless fixing a genuine contract bug (then regen clients as above).

## Git workflow (save work, don't ship)
- In **each** repo, create a branch `audit/overnight` off the current HEAD. Snapshot the current
  working tree as the first commit (`"chore: audit baseline"`) so your changes are reviewable as a diff.
- Commit incrementally with clear messages grouped by finding. End commit messages with:
  `Co-Authored-By: <your model id> <noreply@anthropic.com>`.
- **Do NOT push, do NOT tag, do NOT touch `main`/`origin`.** The user reviews the branches in the morning.
- Keep the localhost:8081 frontend changes on the branch; note them in the report for reversal.

## Gold-standard files to mirror (study these for style + harness)
- Backend tests: `src/inventory/tests.rs`, `src/orders/tests.rs`, `src/stocktakes/tests.rs`,
  `src/purchasing/tests.rs`, `src/reports/tests.rs`, and the e2e in `src/e2e_tests.rs`
  (`test_e2e_purchasing_stocktake_reporting_lifecycle`, `test_milk_swap_converts_units_across_base_units`).
- Harness patterns: seed via raw SQL; mint tokens with `crate::auth::jwt::create_token`; per-user deny
  override for permission-denial tests; assert money in piastres; `BigDecimal` columns compare via
  `.to_string().parse::<f64>()` or `::float8`.
- POS: existing tests under `test/`; dashboard: existing Vitest specs + MSW setup.

## Known recent changes to validate (don't regress these)
Single open shift per teller (DB unique index + login 409 + teller token branch-bound in shifts &
orders); order must attach to the teller's own OPEN shift; catalog unit change rebases all references
(g↔kg/ml↔l only); PO `purchase_unit` restricted to in-family stock units; ingredient swap converts
units; low-stock requires `reorder_threshold>0`; `display_order` removed (render server order).

## Deliverables (write to files in each repo, e.g. AUDIT_REPORT.md)
1. Per-repo + overall **audit report**: each bug found (root cause, file:line, the fix, the test that
   now guards it), edge cases covered, modules audited, test counts before→after, and anything
   deferred (with why). Be explicit about anything you chose NOT to change (design, not bug).
2. **All suites green**, `analyze`/`lint`/`build` clean, on the `audit/overnight` branches.
3. A short "for the human" summary at the top: highest-severity fixes first (money/correctness/security).

Begin by mapping the three repos in parallel, then fan out. Be relentless about edge cases; be
conservative about changes. Never touch prod.
