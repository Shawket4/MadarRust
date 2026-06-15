# Production Release Runbook — Inventory overhaul + order_ref + shift continuity

> Brings **prod** from head `20260612130000` to `20260614030000` — **13 pending migrations** — plus
> two required backfills, then deploys the new backend and clients. **Fully rehearsed** on
> `sufrix_dev`, a fresh prod-data copy taken 2026‑06‑14 (2015 orders, 293 shifts, 8 branches, 4 orgs):
> all 13 migrations apply, both backfills run clean (0 cross-family recipes, 0 duplicate refs), and
> the 330‑test suite is green on the result.
>
> 🔴 Read **§0 Safety** before running anything. Prod is PG 17.10; reachable at
> `100.101.100.57:5432/sufrix` over Tailscale (`srv1460366`).

```bash
# Used throughout. The migrate/backfill steps are the ONLY writes to prod.
PROD='postgres://sufrix:<PWD>@100.101.100.57:5432/sufrix'
BIN=/opt/homebrew/opt/postgresql@17/bin     # pg_dump/psql/pg_restore 17.x
SQLX=~/.cargo/bin/sqlx
cd /Users/shawket/Desktop/SufrixRust         # so sqlx reads ./migrations
```

---

## 0. Safety & the one thing that can bite you

1. **Old binary ✗ new schema, and new binary ✗ old schema.** Phase‑1 migrations **drop**
   `shift_inventory_counts` (20260613003000) and `display_order` columns (20260613006000) — the
   *currently deployed* prod binary (pre‑inventory) still reads those, so it breaks the instant they
   drop. The new binary needs `order_ref`, `branches.code`, `stocktakes`, etc. that don't exist yet.
   ⇒ **The service must be DOWN across the whole migrate+backfill window.** Do not try to migrate
   live.
2. **POS lockstep (hard).** The new backend breaks the *current* POS build (inventory/void/login‑branch
   contract changed, plus order_ref + shift‑continuity). Roll the **new POS build to all tills**
   before/at cutover. The POS is offline‑capable, so tills keep ringing sales during the window and
   sync afterward. The **dashboard** does not hard‑break (additive) but should be redeployed for the
   new fields.
3. **Service‑down = write quiescence**, which is exactly what `backfill-order-ref` needs (it aborts if
   a branch‑day has mixed null/non‑null refs). Don't start the new binary until *after* both backfills
   and phase‑2 have run.
4. **Back up first** (mandatory) — a `-Fc` dump is the rollback.
5. Run every prod command with `$PROD` **explicit**. Never point a migrate/backfill at prod from an
   ambient‑env shell.

---

## 1. Pre‑flight (no downtime — do this before the window)

```bash
# 1a. Confirm prod head is still 20260612130000 (else STOP and reconcile).
$BIN/psql "$PROD" -tAc "SELECT version FROM _sqlx_migrations WHERE success ORDER BY version DESC LIMIT 1;"

# 1b. Guard preconditions (rehearsal: both clean).
$BIN/psql "$PROD" -tAc "SELECT teller_id,count(*) FROM shifts WHERE status='open' GROUP BY 1 HAVING count(*)>1;"
#   → any rows: deliberately close the extra shift(s) (dashboard / admin force‑close) before migrating;
#     20260613011000's unique index will FAIL otherwise. (Rehearsal: none.)

# 1c. Recipe‑unit dry‑run per org — surfaces cross‑family rows that need MANUAL dashboard fixes.
for ORG in 685f6bfa-0d44-4a9f-bb3e-50eec96d50c9 27b8f8db-fec2-4909-b9f6-9fffbd860a1a \
           495760d4-976f-41e3-831b-964b68e68ed7 46066df7-5595-4fde-8292-9676cabd5a03; do
  DATABASE_URL="$PROD" cargo run --bin backfill-recipe-units -- --org "$ORG" --dry-run
done
#   → fix any "Unconvertible (cross‑family)" rows in the dashboard now. (Rehearsal: 0 across all orgs;
#     First Crack had 1 auto‑convertible row only.)

# 1d. Have the new artifacts READY so the window is short:
#   - Backend binary built for x86_64-unknown-linux-gnu (CI on merge to main builds it).
#   - POS new build staged for the tills.
#   - Dashboard build ready (openapi.json + Orval client already regenerated in this repo).
```

---

## 2. The release (maintenance window — pick a low‑traffic time)

### 2a. Back up prod
```bash
$BIN/pg_dump "$PROD" --no-owner --no-privileges -Fc \
  -f sufrix_prod_pre_release_$(date +%Y%m%d_%H%M).dump
```

### 2b. Stop the backend (POS drops to offline mode)
```bash
ssh <vps> 'sudo systemctl stop sufrix-rust'
```

### 2c. Migrations — phase 1 (applies 12, then the order_ref guard stops it — THIS IS EXPECTED)
```bash
DATABASE_URL="$PROD" $SQLX migrate run
```
Applies `20260613000000` … `20260614020000`, then prints:
> `error: while executing migration 20260614030000: orders.order_ref has NULL rows — run cargo run --bin backfill-order-ref …`

That non‑zero exit is **by design** — `order_ref` is now nullable and ready to backfill. Head is
`20260614020000`; `branches.code` is already NOT NULL + populated (auto‑derived by trigger).

### 2d. Backfills (service is down → quiescent)
```bash
# Recipe units — per org (idempotent; fixes already done in 1c).
for ORG in 685f6bfa-0d44-4a9f-bb3e-50eec96d50c9 27b8f8db-fec2-4909-b9f6-9fffbd860a1a \
           495760d4-976f-41e3-831b-964b68e68ed7 46066df7-5595-4fde-8292-9676cabd5a03; do
  DATABASE_URL="$PROD" cargo run --bin backfill-recipe-units -- --org "$ORG"
done

# Order references — dry‑run, then live (rehearsal: 2015 orders, 201 counter groups, 0 dupes).
DATABASE_URL="$PROD" cargo run --bin backfill-order-ref -- --all --dry-run
DATABASE_URL="$PROD" cargo run --bin backfill-order-ref -- --all
```

### 2e. Migrations — phase 2 (finalize order_ref: UNIQUE + NOT NULL)
```bash
DATABASE_URL="$PROD" $SQLX migrate run        # applies 20260614030000
$BIN/psql "$PROD" -tAc "SELECT version FROM _sqlx_migrations WHERE success ORDER BY version DESC LIMIT 1;"
#   → 20260614030000
```

### 2f. Deploy the new backend & bring it up on the migrated schema
Merge `audit/overnight` → `main`; CI builds and runs its deploy step (`systemctl stop` → swap binary →
`start`). Because migrations are already applied, the new binary starts on the correct schema.
*(Shorter window alternative: pre‑build the artifact, `scp` it to `/opt/sufrix-rust/sufrix-rust`, then
`sudo systemctl start sufrix-rust` — skips the CI build wait.)*

### 2g. Ship clients (lockstep)
- **POS:** install the new build on every till (regenerated `sufrix_api`; order_ref shown, shift‑open
  reason flow, void/inventory contract). Tills sync their offline queue once they hit the new backend.
- **Dashboard:** deploy the new build (`npm run generate:api` already done in repo; order_ref replaces
  the order number, shift opening‑edit flag).

---

## 3. Smoke tests (read‑only, after the service is up)
```bash
# order_ref finalized: no nulls, all unique, counters seeded.
$BIN/psql "$PROD" -tAc "SELECT count(*) FILTER (WHERE order_ref IS NULL) AS nulls,
  count(*)-count(DISTINCT order_ref) AS dupes FROM orders;"          -- 0 | 0
$BIN/psql "$PROD" -tAc "SELECT count(*) FROM order_ref_counters;"     -- ~201
$BIN/psql "$PROD" -tAc "SELECT order_ref FROM orders ORDER BY created_at DESC LIMIT 3;"
```
Then via the API: create an order (order_ref minted, e.g. `RUE-260614-0001`); open a shift with the
carried‑over cash (no reason) and with a different amount (reason required → recorded); hit
`GET /reports/orgs/{org}/inventory-valuation`, `…/low-stock`, `GET /inventory/orgs/{org}/settings`.
Confirm a fresh POS sync drains the offline queue.

---

## 4. Rollback
Migrations are forward‑only (phase 1 has the two drops). To revert: restore the §2a dump
(`pg_restore`) and redeploy the previous binary (kept in `/opt/sufrix-rust/backups/`). If only phase 2
misbehaves, it's safe to **defer** it: the new binary reads `order_ref` as nullable, so the system runs
without the UNIQUE/NOT NULL constraint until you re‑run `sqlx migrate run`.

---

## Appendix — the 13 migrations & rehearsal timings
`20260613000000` inventory_movements · `…001000` inventory_permissions · `…002000` stocktakes ·
`…003000` remove_shift_inventory_counts **(drop)** · `…004000` purchasing · `…005000` void_note ·
`…006000` drop_display_order **(drop)** · `…010000` inventory_guardrail_and_gaps · `…011000`
one_open_shift_per_teller · `20260614000000` one_open_stocktake_per_branch · `…010000`
payment_is_cash_snapshot · `…020000` **order_ref (phase 1)** · `…030000` **order_ref_finalize (phase 2)**.

Rehearsal (sufrix_dev, fresh prod copy): all 12 phase‑1 migrations <100 ms total; recipe‑units 0
cross‑family / 1 convertible; order‑ref 2015 orders / 201 groups / **0 dupes**; phase‑2 applied in
~5 ms; `cargo test` → **330 passed**.
