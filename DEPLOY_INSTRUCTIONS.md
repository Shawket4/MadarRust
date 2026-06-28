# Deploy — backend + migrations (inventory overhaul + teller shift-race fix)

Brings **production** from its current head (`20260612130000`) to `20260613011000`, deploys the
new backend binary, and regenerates the API clients. Every migration is additive/data-preserving.
Validated end-to-end on `madar_dev` (a fresh prod copy): all 286 lib tests pass and the full
migration chain applies cleanly.

> 🔴 `.env` `DATABASE_URL` points at **PROD**. The migrate step is the only time prod is written.
> Always pass the prod URL explicitly; never run a migrate/backfill from a shell whose env points
> at prod by accident. Take a backup first.
>
> 🔴🔴 **CLIENT LOCKSTEP — the current POS build will BREAK against this backend.** This release
> bundles the inventory overhaul, which **removed `display_order`** from 8 entities (categories,
> menu items, item sizes, addon items, addon slots, optional fields, bundles, payment methods).
> The POS's generated `madar_api` (built_value) marks `display_order` **required**, so the missing
> field throws on deserialize → **menu/categories/addons/bundles/payment-methods fail to load → the
> POS cannot sell.** You **must** ship a new POS build (regenerated `madar_api` from the new
> `openapi.json`, + the login/branch changes) **in lockstep** with this deploy — do not deploy the
> backend to prod while tills run the old build. The **dashboard** (React/Orval/TS) does *not* hard-
> break (TS is compile-time; stale fields are just `undefined` at runtime) — it works but should be
> regenerated too. See Step 6.

---

## 0. Backup (mandatory)
```
PROD='postgres://madar:<PWD>@100.101.100.57:5432/madar'
/opt/homebrew/opt/postgresql@17/bin/pg_dump "$PROD" --no-owner --no-privileges -Fc \
  -f madar_prod_pre_deploy_$(date +%Y%m%d_%H%M).dump
```

## 1. Verify head
```
psql "$PROD" -tA -c "SELECT version FROM _sqlx_migrations WHERE success ORDER BY version DESC LIMIT 1;"
# expect 20260612130000
```

## 2. 🔴 Pre-step for `20260613011000` — resolve duplicate open shifts
The teller-shift fix adds a unique index so a teller can hold only one open shift. It does **not**
auto-close anything. If any teller currently has >1 open shift, the index creation FAILS until you
fix it. Find offenders:
```
psql "$PROD" -tA -c "SELECT teller_id, count(*), array_agg(branch_id) \
  FROM shifts WHERE status='open' GROUP BY teller_id HAVING count(*) > 1;"
```
For each, close the **extra** shift(s) deliberately — keep the one the teller is actually working:
- Preferred: the dashboard shift screen, or `POST /shifts/{shift_id}/force-close` (admin) so cash is
  reconciled/audited.
- The known offender in the prod copy was one teller open at two branches; expect a small number.

Re-run the query until it returns nothing.

## 3. Apply migrations (the prod write)
From the repo root (`/Users/shawket/Desktop/MadarRust`):
```
cd /Users/shawket/Desktop/MadarRust
DATABASE_URL="$PROD" ~/.cargo/bin/sqlx migrate run
```
Applies, in order:
```
20260613000000 inventory_movements        20260613005000 void_note
20260613001000 inventory_permissions      20260613006000 drop_display_order
20260613002000 stocktakes                 20260613010000 inventory_guardrail_and_gaps
20260613003000 remove_shift_inventory_counts  20260613011000 one_open_shift_per_teller
20260613004000 purchasing
```
Confirm:
```
psql "$PROD" -tA -c "SELECT version FROM _sqlx_migrations WHERE success ORDER BY version DESC LIMIT 1;"
# → 20260613011000
psql "$PROD" -tA -c "SELECT indexname FROM pg_indexes WHERE indexname='idx_shifts_one_open_per_teller';"
```
Requires **PostgreSQL 12+** (`20260613010000` uses `ALTER TYPE … ADD VALUE`; prod is 17.10).

## 4. Backfills (unit correction)
See `INVENTORY_PROD_MIGRATION_RUNBOOK.md` §3–4. In short, **dry-run first**, then live, per org:
```
DATABASE_URL="$PROD" cargo run --bin backfill-recipe-units -- --org <ORG_UUID> --dry-run
DATABASE_URL="$PROD" cargo run --bin backfill-recipe-units -- --org <ORG_UUID>
# optional, reprice historical COGS:
DATABASE_URL="$PROD" cargo run --bin backfill-cost-snapshots -- --org <ORG_UUID> --dry-run
```

## 5. Deploy the backend binary
Build and ship the binary from this repo (it expects the new schema — deploy it **with** the
migrations, not before). The OpenAPI spec (`openapi.json`, 702 KB) is current.

## 6. Regenerate API clients
- **Dashboard:** `npm run generate:api` (Orval, reads `../MadarRust/openapi.json`). See
  `MadarDashboard/INVENTORY_FRONTEND_HANDOFF.md` and `MadarDashboard/SHIFT_AUTH_FIX_HANDOFF.md`.
- **POS (MANDATORY — old build breaks):** regenerate the `madar_api` dart package from the new
  `openapi.json` (`tool/codegen.sh` / `tool/generate_api.sh`), then rebuild and release the app.
  Regeneration drops the now-removed `display_order` from the models (so it's no longer required) and
  adds the new inventory models. The login error-state + fetched-branch-name changes ship in the same
  build. **Do not roll the backend to prod before this POS build is ready to ship.**

> Scope note: the **shift / login / order-binding** changes by themselves are wire-compatible (only
> new **409/403** outcomes, covered by the shared `ErrorBody`) and degrade gracefully on an old
> client. The breaking part is the bundled **inventory overhaul** (`display_order` removal etc.),
> which is why the POS must regenerate + rebuild. Sequence the cutover so backend + POS go together.

## 7. Smoke test (prod, low risk)
- `GET /reports/orgs/{org}/inventory-valuation`, `/low-stock`, `/inventory/orgs/{org}/settings`.
- Open a shift on a test teller; attempt a second login for that teller → expect **409**.
- With a token for branch A, hit `/shifts/branches/{B}/current` → expect **403**.

## Rollback
Migrations are additive (forward-only). To revert, restore the Step-0 dump (`pg_restore`). The only
destructive migrations in the set drop `shift_inventory_counts` and the `display_order` columns —
both preserved in the pre-deploy dump.
