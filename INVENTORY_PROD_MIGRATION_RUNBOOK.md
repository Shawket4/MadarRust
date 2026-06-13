# Production Migration Runbook — Foodics Inventory

> Operator runbook to bring **production** from its current schema up to the full
> Foodics-grade inventory system. Every migration here is **additive and
> data-preserving** and was validated on `sufrix_dev` (a fresh prod-data copy) —
> all 252 lib tests pass and the new SQL runs clean on real rows.
>
> 🔴 **Read the safety section before running anything.**

---

## State of the world

| Environment | At migration | Notes |
|---|---|---|
| **dev** (`localhost:5433/sufrix_dev`) | `20260613010000` | fully migrated + recipe-unit backfill applied; tested |
| **prod** (`100.101.100.57:5432/sufrix`) | `20260612130000` | **missing the 8 migrations below** |

Prod is missing the entire `20260613*` set (the inventory overhaul) **plus** this pass's
`20260613010000` (variance guardrail + gap closers):

```
20260613000000_inventory_movements.sql
20260613001000_inventory_permissions.sql
20260613002000_stocktakes.sql
20260613003000_remove_shift_inventory_counts.sql
20260613004000_purchasing.sql
20260613005000_void_note.sql
20260613006000_drop_display_order.sql
20260613010000_inventory_guardrail_and_gaps.sql   ← inventory guardrail + gaps
20260613011000_one_open_shift_per_teller.sql       ← teller shift-race fix (see note)
```

> ⚠️ **`20260613011000` needs a manual pre-step.** It adds a unique partial index so a teller can
> only ever have one open shift — but it does **NOT** auto-close anything (closing a shift settles
> cash, so it must be a deliberate, audited action). If any teller currently has more than one open
> shift, the `CREATE INDEX` will FAIL until you resolve them. **Before running migrations:**
> ```
> SELECT teller_id, count(*) FROM shifts WHERE status='open' GROUP BY 1 HAVING count(*) > 1;
> ```
> For each offender, close the extra shift(s) deliberately — via the dashboard's shift screen or an
> admin force-close — keeping the one the teller is actually working. Then run migrations.
> After deploy the backend also: blocks **any** login while the user has an open shift, and binds a
> teller's token to the branch they signed into (shifts **and** orders). See the deploy doc
> `DEPLOY_INSTRUCTIONS.md`.

Requirements: **PostgreSQL 12+** (`20260613010000` uses `ALTER TYPE … ADD VALUE` inside the
migration transaction — fine on PG12+; prod is well past that). `sqlx-cli` available
(`~/.cargo/bin/sqlx`), run from the repo root so it reads `./migrations`.

---

## 🔴 Safety

1. **`.env` `DATABASE_URL` points at PROD.** This runbook is the **only** time prod is written.
   Always pass the prod URL explicitly on the command; never rely on ambient env for anything
   destructive, and never point a `migrate`/backfill at prod by accident from a dev shell.
2. **Back up first** (mandatory):
   ```
   /opt/homebrew/opt/postgresql@17/bin/pg_dump \
     'postgres://sufrix:<PWD>@100.101.100.57:5432/sufrix' \
     --no-owner --no-privileges -Fc -f sufrix_prod_pre_inventory_$(date +%Y%m%d_%H%M).dump
   ```
3. **Maintenance window recommended.** The migrations are quick (dev applied each in <10 ms) and
   additive, but `20260613003000` drops the legacy `shift_inventory_counts` table and
   `20260613006000` drops `display_order` columns — confirm nothing prod-side still reads them
   (the deployed backend in this repo already doesn't).
4. **Backend + clients:** deploy the new backend binary built from this repo **with** the
   migrations (the running server expects the new schema). The POS void flow is already on the new
   contract; regenerate the dashboard Orval client and the POS `sufrix_api` package from the new
   `openapi.json` after deploy.

---

## Step 1 — verify prod's current migration head

```
PROD='postgres://sufrix:<PWD>@100.101.100.57:5432/sufrix'
/opt/homebrew/opt/postgresql@17/bin/psql "$PROD" -tA \
  -c "SELECT version, description FROM _sqlx_migrations WHERE success ORDER BY version DESC LIMIT 3;"
```
Expect the head to be `20260612130000`. If it differs, **stop** and reconcile before continuing.

## Step 2 — apply the migrations (the prod write)

From the repo root (`/Users/shawket/Desktop/SufrixRust`):
```
cd /Users/shawket/Desktop/SufrixRust
DATABASE_URL="$PROD" ~/.cargo/bin/sqlx migrate run
```
`sqlx` applies only the not-yet-applied files, in order, each in its own transaction. Confirm:
```
/opt/homebrew/opt/postgresql@17/bin/psql "$PROD" -tA \
  -c "SELECT version FROM _sqlx_migrations WHERE success ORDER BY version DESC LIMIT 1;"
# → 20260613011000
```
Spot-check the new objects:
```
psql "$PROD" -tA -c "SELECT enumlabel FROM pg_enum WHERE enumtypid='purchase_order_status'::regtype;"   -- incl. partially_received
psql "$PROD" -tA -c "SELECT 1 FROM information_schema.columns WHERE table_name='org_ingredients' AND column_name='supplier_id';"
psql "$PROD" -tA -c "SELECT stocktake_variance_threshold_pct FROM organizations LIMIT 1;"   -- 10.000
```

## Step 3 — backfill: normalize recipe units (corrects inventory units)

Recipe quantities must be stored in each ingredient's base stock unit. Run **dry-run first**,
fix any reported cross-family rows manually, then run live. (Already done on dev.)
```
# Inspect — makes NO changes:
DATABASE_URL="$PROD" cargo run --bin backfill-recipe-units -- --org <ORG_UUID> --dry-run
# Review the "converted" and especially the "unconvertible (cross-family)" report.
# Fix unconvertible rows by editing those recipes in the dashboard, then:
DATABASE_URL="$PROD" cargo run --bin backfill-recipe-units -- --org <ORG_UUID>
```
Run per org (or use `--branch <UUID>`). Cross-family mismatches (e.g. a liquid recipe line in
grams) are **reported, not auto-fixed** — they need a human decision.

## Step 4 — (optional) reprice historical order cost snapshots

Only if you want past `order_items` cost/COGS recomputed at current recipes & ingredient costs
(mirrors the menu-engineering `cost_basis=current` view). **Dry-run first.**
```
DATABASE_URL="$PROD" cargo run --bin backfill-cost-snapshots -- --org <ORG_UUID> --dry-run
DATABASE_URL="$PROD" cargo run --bin backfill-cost-snapshots -- --org <ORG_UUID>
```

## Step 5 — deploy + regenerate clients

1. Deploy the backend binary built from this repo.
2. Regenerate API clients from the new `openapi.json` (702 KB, in this repo):
   - Dashboard: `npm run generate:api` (Orval) — see `SufrixDashboard/INVENTORY_FRONTEND_HANDOFF.md`.
   - POS: regenerate the `sufrix_api` dart package.
3. Smoke-test on prod (low risk, read-only): hit `GET /reports/orgs/{org}/inventory-valuation`,
   `GET /reports/orgs/{org}/low-stock`, `GET /inventory/orgs/{org}/settings`.

---

## Rollback

Migrations are additive, so forward-only is safe. If you must revert:
- Fastest/safest: restore the Step-2 `pg_dump` (`pg_restore`).
- The destructive bits to be aware of when restoring: `shift_inventory_counts` (dropped) and the
  `display_order` columns (dropped). The pre-migration dump preserves both.

## What this delivers on prod
Movement ledger, standalone stock counts with the **variance guardrail** (suspicious differences
require a reason; default 10% org-tunable via `/inventory/orgs/{id}/settings`), waste, purchasing
with **partial multi-shipment receiving** + weighted-average cost, **ingredient→supplier** links,
**de-noised low-stock**, a **last-counted** signal, org-wide PO list + report rollups, a
**shrinkage-by-reason** report, and consistent `inventory/read` gating on inventory reports.
