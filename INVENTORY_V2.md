# Inventory v2 — count-first, ledger-derived stock

Landed on branch `inventory/v2` (backend) + `inventory/v2` (dashboard), migration
`migrations/20260906000000_inventory_v2.sql`. Companion audit:
`~/ClaudeProjects/INVENTORY_AUDIT_2026-09-05.md`.

## The model in one paragraph

The **org catalog is the only setup**. A branch has no "tracked ingredients" list any
more: every catalog ingredient is countable, wastable and reportable at every branch
from day one. A `branch_stock` row (balance, actual cost, par levels, last activity) is
created **lazily** by the first movement or the first par setting and is never a
precondition — every read is `org_ingredients LEFT JOIN branch_stock`. Quantities change
**only** through `inventory_movements`: a `BEFORE INSERT` trigger upserts the balance and
stamps `balance_after` / `below_zero` on the movement, and a guard trigger rejects any
other write to `on_hand`. Stock counts are the front door: a count lists the whole
catalog (or a category / chosen items), measures every difference against **book stock
at finalize** (not the opening snapshot), and posts one `stock_count` movement per
difference.

## Schema

| Table | What changed |
|---|---|
| `ingredient_categories` (new) | org-scoped, `slug` is the stable key (`milk` / `coffee_bean` carry menu swap semantics). `general` exists for every org (seeded + `AFTER INSERT` trigger on organizations). `ingredient_category_id(org, slug)` gets-or-creates. |
| `org_ingredients` | `category text` → `category_id NOT NULL` FK. |
| `branch_stock` (was `branch_inventory`) | `current_stock` → `on_hand`; `reorder_threshold` folded into `par_min` and dropped; `last_counted_at`, `last_movement_at`; par checks. |
| `inventory_movements` | `branch_inventory_id` → `branch_stock_id`; `balance_after NOT NULL` (back-filled by reverse running sum); `trg_inventory_movements_apply` moves the balance. |
| `branch_stock` guard | `trg_branch_stock_on_hand_guard` raises on any direct `on_hand` write. The single sanctioned exception is a unit re-denomination under `SET LOCAL madar.stock_rebase = 'on'`. |
| `stocktakes` / `stocktake_items` | `scope jsonb`; `expected_qty` → `opening_qty`, `system_qty` → `book_qty`; `variance = counted − COALESCE(book, opening)`. |
| `stock_transfers` (was `branch_inventory_transfers`) | rename only. |

Data preservation is asserted inside the migration (row counts, `SUM(on_hand)`, every
org has `general`). Rehearsed on a restored prod copy.

## API (see `openapi.json`)

- `GET/POST /inventory/orgs/{org}/categories`, `PATCH/DELETE …/categories/{id}`
  (`?reassign_to=` required when ingredients use it; `general` is undeletable).
- Catalog create/update take `category_id`; responses carry `category_id/slug/name`.
- `GET /inventory/branches/{b}/stock` → every catalog ingredient as `BranchStockRow`
  (`on_hand`, `par_min/max`, `below_par`, `last_counted_at`, `last_movement_at`, `has_activity`).
- `PUT /inventory/branches/{b}/stock/{ingredient}/par` replaces the old add/update/remove
  stock endpoints. **No endpoint sets `on_hand`.**
- Stocktakes: snapshot from the catalog; items carry `opening_qty`, live `book_qty`,
  `is_new`; finalize locks by `(branch, ingredient)`, posts `stock_count` movements and
  stamps `last_counted_at`. Variance rows carry `opening_qty` + `book_qty`.
- Low stock (`/reports/…/low-stock`) uses `par_min > 0 AND on_hand <= par_min`; rows carry
  `par_min`, `par_max`, `suggested_qty`.
- Sales, void restock, delivery cancel-waste, purchase receipts/returns, waste and
  transfers all post movements through `inventory::movements::record_movement`.
  A sale on an ingredient the branch never saw creates its balance row (negative, flagged).

## Rollout

1. Take a dump: `pg_dump -Fc --no-owner --no-privileges -h localhost -U madar -d madar -f ~/madar_prod_$(date +%F).dump`.
2. Stop the backend. Start the new binary — it runs the migration on boot (renames mean
   the old binary cannot serve the new schema; no operator backfill is needed).
3. Deploy the dashboard build (`npm run generate:api` is already committed).
4. Smoke: open a branch that was never counted → Today shows "Start by counting this
   branch"; Stock counts → Start a count lists the whole catalog.

## Dev

- Local Postgres 17, DB `madar` (restored prod copy, migrated), `madar_local` for
  `cargo test`. `DATABASE_URL=postgres://apex:apex@localhost:5432/madar cargo test --lib`.
- Dashboard: `npm run dev` with `.env.local` → `http://localhost:8081`.
