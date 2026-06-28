# Madar Backend — Overnight Audit Report

**Branch:** `audit/overnight` (off `main` @ `10dc120`)
**Scope:** Bug/edge-case hardening of the Rust backend (Actix-Web + SQLx + Postgres).
**Test status:** **315 passing / 0 failing** (baseline was **289**; +26 new regression tests). Build clean, no OpenAPI contract drift (no client regeneration needed).
**DB safety:** Production (`100.101.100.57`) was never touched. All test runs used the ephemeral superuser DB (`postgres@localhost:5432/madar_local`, built from `./migrations`); the dev copy (`localhost:5433`) was used **read-only** for schema ground truth.

## Method
1. **Map** — 27 parallel readers over every backend module + POS + dashboard produced 172 suspicious spots.
2. **Verify** — 33 deduped backend candidates were adversarially re-checked against the **actual Postgres schema** (dumped read-only from the dev prod-copy: unique indexes, CHECK constraints, generated columns, `numeric(p,s)` precision, enums) — not just the Rust code. **30 confirmed real bugs, 3 cleared.**
3. **Fix** — minimal, bug-only changes, each with a red→green regression test, applied in module batches with the suite kept green throughout.

---

## 🔴 For the human — read this first (highest severity)

| # | Severity | Area | One-liner |
|---|----------|------|-----------|
| **V1** | **HIGH / security** | reports | **Stored SQL injection** via an unvalidated branch `timezone` interpolated into the sales-timeseries query. Fixed (parameterized + write-time validation). No live exploit (prod only has `Africa/Cairo`). |
| **V17** | **HIGH / money** | reports | Split-payment orders **double/triple-counted revenue, tax, discounts** in shift-summary & branch-comparison via an unused fan-out join. Fixed. |
| **V13** | **HIGH / money** | orders/shifts | An order could attach **cash to a just-closed shift** (TOCTOU) → money missing from `closing_cash_system`. Fixed with a shared per-shift advisory lock + in-tx recheck. |
| **V10** | **HIGH / money** | purchasing | A cheap-per-gram ingredient (e.g. 400 piastres/kg = 0.40/g) had its cost **rounded to 0 → treated as free** in COGS. Fixed (2-dp Decimal). |
| **V19** | **HIGH / units** | orders | Bundle-component milk/coffee swap deducted the recipe quantity **without g↔kg / ml↔l conversion → up to 1000× over-deduction** + COGS inflation. Fixed. |
| **V6** | money | orders | Concurrent/retried void **double-restocked inventory**. Fixed (idempotent guarded UPDATE). |
| **V15** | money | orders | A percentage discount >100 (or negative) produced a **negative/inflated total & tax**. Fixed (clamp). |
| **V3/V4/V26/V5** | security | users/inventory/uploads | Cross-tenant branch assignment, branch-manager → org-admin **password takeover**, teller branch-binding gaps, and **cross-tenant file deletion**. All fixed. |

Two confirmed issues were **deliberately NOT changed** (design / risk) and are documented at the bottom (**V28** stateless-JWT revocation, **V30** payment-method-rename cash history). The **POS & dashboard** findings (money formatting, error surfacing) were catalogued but **not modified** this run — see *Frontend findings (deferred)*.

---

## Confirmed bugs fixed (30)

Each entry: root cause → fix (file) → guarding test.

### Money / correctness

- **V1 — SQL injection via branch timezone** (`reports/handlers.rs:538-588`).
  `branch.timezone` (free text, no validation) was raw-interpolated via `format!` into `AT TIME ZONE '{tz}'`. **Fix:** bind it as `$4`; `trunc` stays interpolated (closed enum whitelist). Plus defense-in-depth `validate_timezone()` against `pg_timezone_names` on branch create/update (`branches/handlers.rs`).
  Tests: `reports::test_timeseries_timezone_is_bound`, `branches::test_create_branch_rejects_invalid_timezone`.

- **V17 — split-payment report inflation** (`reports/handlers.rs` shift_summary, org_branch_comparison).
  A spurious `LEFT JOIN order_payments` fanned out every order-level aggregate by the number of payment rows. The join was unused (revenue_by_method has its own subquery). **Fix:** delete the join.
  Tests: `reports::test_shift_summary_split_payment_not_double_counted`, `test_org_branch_comparison_split_payment_revenue`.

- **V18 — consumption counted voided sales** (`reports/handlers.rs` branch/org consumption).
  Summed `type IN ('sale','waste')` but not `void_restock`, so a voided-and-restocked sale still counted as consumed. **Fix:** include `void_restock` (its positive qty nets the sale).
  Test: `reports::test_consumption_nets_voided_restock`.

- **V31 — voided discounts/tips inflated order summaries** (`orders/handlers.rs:1617, 2471`).
  Revenue/counts were status-filtered but `SUM(discount_amount)`/`SUM(tip_amount)` were not. **Fix:** status-filter them to `completed`.
  Test: `orders::test_summary_excludes_voided_discounts`.

- **V15 — unbounded discount** (`orders/handlers.rs:1076-1084`).
  Percentage >100 or negative value drove `taxable`/`tax`/`total` negative or inflated. **Fix:** `discount_amount.clamp(0, subtotal)`.
  Tests: `orders::test_discount_percentage_over_100_is_clamped`, `test_discount_negative_value_is_clamped`.

- **V10 — cheap cost rounded to 0/free** (`purchasing/handlers.rs:565`).
  `(unit_cost / factor).round() as i64` zeroed sub-piastre per-base-unit costs. **Fix:** keep 2-dp `Decimal` for `cost_per_unit` (the `numeric(15,2)` column holds it); the bigint movement ledger keeps a rounded whole-piastre snapshot.
  Test: `purchasing::test_receive_cheap_cost_not_rounded_to_zero`.

- **V9 — WAC truncated to whole piastres** (`costing/service.rs:56-70`).
  The blended cost was `.round()`ed to an integer before storing into `numeric(15,2)`. **Fix:** blend in `Decimal`, store 2 dp. (e2e WAC `15.0` stays green.)

- **V19 — bundle-component unit swap not converted** (`orders/component_resolve.rs:211`).
  Reassigned the unit without converting the quantity (the milk→almond class). **Fix:** `units::convert` before swapping, mirroring the direct-item path; cross-family swaps skip the deduction.
  Test: `orders::test_bundle_component_swap_converts_units`.

- **V20 — cross-family addon swap mis-deducted** (`orders/handlers.rs:931`).
  On an incompatible-family swap it logged a warning and deducted the unconverted quantity. **Fix:** skip the deduction (leave untracked) instead of corrupting stock/COGS.

- **V22 — recipe quantity rounding to 0** (`recipes/handlers.rs:361`).
  A positive quantity that normalizes to 0 in the base unit (0.4 g into a kg-base ingredient) was silently stored as a no-op line. **Fix:** reject it.
  Test: `recipes::test_drink_recipe_subunit_rounding_to_zero_rejected`.

- **V24 — bundle margin-floor bypass** (`bundles/handlers.rs:210`).
  `compute_item_cost` INNER-JOINed `org_ingredients`, dropping unlinked recipe lines → undercounted cost → under-priced bundles passed the 1.20× floor. **Fix:** LEFT JOIN, return `None` for unknown cost, and **block activation** on unknown (drafts still allowed).
  Test: `bundles::test_bundle_activation_blocked_on_unknown_component_cost`.

- **V25 — bundle_performance used current (not snapshot) cost** (`bundles/handlers.rs:1149`).
  Re-read live `org_ingredients.cost_per_unit` and zeroed unknowns. **Fix:** compute net profit from the sale-time `order_items.line_cost` over `cost_missing = false` lines.
  Test: `bundles::test_bundle_performance_uses_snapshot_cost`.

- **V16 — unbounded tax_rate** (`orgs/handlers.rs:197, 346`). **Fix:** validate `0 ≤ tax_rate ≤ 1`.
  Test: `orgs::test_update_org_rejects_out_of_range_tax_rate`.

### Concurrency / TOCTOU

- **V13 — order attaches to a closing shift** (`orders/handlers.rs:444` + `shifts/handlers.rs:642`).
  `close_shift` snapshotted cash *outside* its tx with no lock while `create_order` held a per-shift advisory lock. **Fix:** `close_shift` now takes the **same** advisory lock and snapshots cash *inside* its tx; `create_order` re-checks the shift is open under that lock.
  Test: `orders::test_order_rejected_on_closed_shift` (contract); race closed by the shared lock.

- **V6 — concurrent void double-restock** (`orders/handlers.rs:1721`).
  Void UPDATE had no status precondition. **Fix:** `WHERE id=$1 AND status <> 'voided'` + `fetch_optional` → idempotent.
  Test: `orders::test_void_is_idempotent_no_double_restock`.

- **V7 — PO double-receive** (`purchasing/handlers.rs:546`).
  Status checked before `begin()`, no row lock. **Fix:** `SELECT … FOR UPDATE` the PO row and re-check status inside the tx.

- **V8 — duplicate line_id in one receive** (`purchasing/handlers.rs`). **Fix:** reject duplicate `line_id`s.
  Test: `purchasing::test_receive_rejects_duplicate_line_id`.

- **V11 — stocktake double-finalize** (`stocktakes/handlers.rs:383`).
  Status read outside the tx; UPDATE unguarded → double-posted `stock_count` movements. **Fix:** `FOR UPDATE` the stocktake + re-check status inside the tx. (Existing `test_finalize_already_finalized_conflict` guards the contract.)

- **V12 — two open stocktakes per branch** (`stocktakes/handlers.rs:152`).
  App-only check. **Fix:** partial unique index `idx_stocktakes_one_open_per_branch` (migration `20260614000000`) + 23505→409.

- **V27 — idempotency-key race returned 500** (`orders/handlers.rs:1129`).
  **Fix:** on a 23505 for the idempotency index, replay the existing order.
  Test: `orders::test_idempotency_key_replays_same_order`.

- **V14 — split payments unvalidated** (`orders/handlers.rs:1174`).
  **Fix (conservative):** reject non-positive split amounts. (We deliberately do **not** enforce `sum == total` here — the POS may legitimately split against a pre-tax subtotal; enforcing it backend-side would reject valid orders. Flagged for a coordinated POS+backend change.)
  Test: `orders::test_split_payment_rejects_nonpositive_amount`.

### Security / multi-tenant isolation

- **V2 — cross-tenant discount** (`orders/handlers.rs:471`). Discount lookup wasn't org-scoped. **Fix:** add `AND org_id = $2`.
  Test: `orders::test_discount_id_must_belong_to_caller_org`.

- **V3 — cross-tenant branch assignment** (`users/handlers.rs` assign/unassign_branch). No org check. **Fix:** `require_same_org` for both the target user and the branch.
  Test: `users::test_assign_branch_cross_org_forbidden`.

- **V4 — privilege escalation (password takeover)** (`users/handlers.rs` update_user / assign_branch).
  A branch_manager could reset an org_admin's credentials on a shared branch. **Fix:** rank-based guard — a caller may only mutate credentials/role/status of a **strictly lower-privileged** user; a branch_manager can't attach an admin to a branch.
  Test: `users::test_branch_manager_cannot_reset_org_admin_password`.

- **V26 — teller branch-binding gaps** (`require_branch_access` in inventory/purchasing/stocktakes/reports). The teller token→branch binding was only enforced in orders/shifts. **Fix:** add the identical guard to all four.
  Test: `inventory::test_teller_token_branch_binding_on_inventory`.

- **V5 — cross-tenant file deletion** (`uploads/handlers.rs:167`). `delete_old_image` only checked `uploads_root`, so a crafted `image_url` could delete another org's file. **Fix:** scope deletion to `{uploads_root}/{org_id}` for menu/category images (logos stay root-scoped, super-admin only).
  Tests: `uploads::test_delete_old_image_blocks_cross_tenant`, `test_delete_old_image_blocks_escape`.

- **V32 — calibration mis-attributed a base-price epoch to a sized SKU** (`menu_advisor/persistence.rs:769`). **Fix:** match by `COALESCE(size_label,'one_size')`. (Existing one_size calibration test guards the regression.)

---

## Reviewed and NOT changed

**Cleared by adversarial verification (not bugs):**
- **V33 — order_number MAX+1 race** — already guarded by `pg_advisory_xact_lock(hashtext(shift_id))` before the numbering.
- **V23 — `units::convert` negative qty** — unreachable; no caller can pass a negative quantity (recipe/order inputs are validated `> 0`).
- **V29 — cancelling a partially-received PO leaves received stock** — correct by design (received goods are physically in inventory).

**Initially deferred, then fixed on request:**
- **V28 — deactivated/soft-deleted user keeps access until JWT expiry → FIXED.**
  `check_permission` now resolves `is_active AND deleted_at IS NULL` for the token's user and rejects `Some(false)` (a disabled/deleted account) with 403. A **missing** row still passes (so service/integration tokens and tests that mint tokens for non-stored users keep working) — a deactivated *real* user is stopped immediately. Guarded by `permissions::test_disabled_user_token_is_rejected`.
- **V30 — renaming a payment method / flipping `is_cash` corrupts historical shift cash → FIXED.**
  Added `order_payments.is_cash` and `orders.tip_is_cash`, snapshotted at sale time (migration `20260614010000`, with best-effort backfill). `close_shift` and the shift report now read those snapshots (with a `method='cash'` fallback for legacy rows) instead of joining `org_payment_methods` by name, so later config changes can't rewrite a closed shift's cash. Guarded by `orders::test_order_payment_snapshots_is_cash` + `shifts::test_close_cash_uses_is_cash_snapshot`.

**Money-math alignment (frontend ↔ backend):**
- Percentage discount and tax now **round** (half away from zero) instead of truncating, matching the POS's rounded preview to the piastre (`orders::test_percentage_discount_is_rounded_not_truncated`). The dashboard's `egpToPiastres` was likewise switched to `Math.round`. Verified the generated clients are in sync: regenerating the dashboard Orval client and the POS `madar_api` package against the current `openapi.json` produced no contract change.

**Tax made first-class across the stack (feature, on request):**
- `/auth/login` + `/auth/me` now return the org `tax_rate` + `currency_code` (OpenAPI regenerated; both generated clients regenerate to no-diff → in sync).
- **POS** computes a tax-inclusive cart total matching the backend exactly (`round((subtotal−discount)×rate)`), validates tender/splits against it, shows a Tax line. Backward-compatible (rate 0 ⇒ unchanged). `flutter test` 325/0.
- **Dashboard** surfaces tax: order detail (already there), an orders-list Tax column, an analytics "Tax" stat (`total_tax`); menu engineering is noted pre-tax (tax is order-level, not per-item).

**Still genuinely deferred (design, not a defect):**
- **V29 — cancelling a partially-received PO leaves received stock** — correct by design.

---

## Frontend (POS + dashboard) — done in a follow-up pass

The frontend was subsequently audited and fixed; full details in each repo's `AUDIT_REPORT.md`. Summary:

**Dashboard** (`MadarDashboard/AUDIT_REPORT.md`) — `tsc`/`eslint` clean, Vitest green:
- **Fixed: `egpToPiastres` used `Math.trunc`** → dropped a piastre on ~5.7% of two-decimal prices (`19.99*100 = 1998.999…` → 1998). Switched to `Math.round` (+ the inline editable-card / bulk-price paths), guarded by a new test.
- **Fixed: the Vitest harness was broken** (config referenced a missing `src/test/setup.ts`; zero tests) → added the setup file + first test; `npm run test` now runs.
- Documented (not changed): read-query empty-state error surfacing, 403-on-GET banner, void detail-cache invalidation, super_admin org switcher.

**POS** (`madar_pos/AUDIT_REPORT.md`) — `flutter test` **318 pass / 0 fail** (was **13 failing on baseline**), `flutter analyze` clean:
- **Fixed: 13 stale tests** (test rot, no `lib/` change) from recent refactors — `display_order` removed (now server-order rendering), the open-shift redirect flow (`/open-shift`), and a new `PendingVoidOrder.note`. Each was verified against the shipped models; none masked a code bug.
- Documented (not changed, with rationale): cart total omits tax + split/tender validate against the tax-free total — **no current impact (every org has `tax_rate = 0`)** and a correct fix is a *feature* (the POS has no tax model); swallowed 403/409 login errors; percentage-discount round-vs-truncate (1 piastre); offline-void-of-offline-order id mismatch.

(No runtime API-base / `.env` changes were made in any repo.)

---

## Coverage

Modules audited (all green): auth, orders, shifts, inventory, stocktakes, purchasing, costing, recipes, units, reports, permissions, menu, menu_advisor, bundles, discounts, orgs, branches, payment_methods, users, uploads, e2e.

New regression tests by module: reports +4, orders +9, purchasing +2, recipes +1, users +2, inventory +1, branches +1, orgs +1, bundles +3, menu +1, uploads +2 = **+26** (289 → 315).

New migration: `migrations/20260614000000_one_open_stocktake_per_branch.sql` (partial unique index; applied automatically by the `#[sqlx::test]` harness and any normal deploy — **not** run against prod).
