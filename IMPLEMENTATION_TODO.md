# IMPLEMENTATION_TODO — Wave 2 backend rewrite (menu unification)

Repoint every legacy-table reference onto the new unified schema (see `CONTRACT.md`), wire the compat
shim, retire the divergent bundle cost engine, and regenerate the OpenAPI spec + `.sqlx` cache. The
EXPAND migration, backfill, and shim views already exist and are validated.

## ⭐ STRATEGY UPDATE (this is smaller than the raw grep implies)

**Read-only code can keep querying the LEGACY table names.** In prod, the shim
(`deploy/menu_unification_shim.sql`) turns those names into VIEWS over the new tables, so every
`SELECT` returns live new-table data with no code change. In tests (no shim), the legacy tables are real
(created by earlier + the additive expand migration) and seeded by existing fixtures, so read code +
read tests pass UNCHANGED. **→ the 9-file read-fixture cascade is NOT required.** The flip reduces to:

- **NEW additive endpoints on the new tables** (the new authoring/POS surface):
  - [x] **Menu Studio** (`src/menu/studio.rs`) — `GET /menu-items/{id}/studio`, `PUT …/sizes`,
        `PUT /menu-item-sizes/{id}/recipe`, `PUT …/modifier-groups`, `POST …/duplicate`. Reads/writes new
        tables; bumps `catalog_revision`; 6 tests; full crate green (572 tests). DONE.
  - [x] **`GET /catalog/sync`** (`src/menu/catalog_sync.rs`) — POS snapshot, effective price/availability
        per §3, revision-gated. DONE (7 tests).
  - [x] **Remaining Menu Studio writes** (`src/menu/modifiers.rs`, 12 endpoints): reusable
        `modifier-groups`/`options` CRUD + `PUT /modifier-options/{oid}/recipe`, `PUT /menu-items/{id}/options`
        (priced optionals), `PUT/DELETE /menu-price-overrides`, `GET /menu-items/{id}/cost`. DONE (7 tests).
- **WRITE paths that must target new tables** (because their shim views are JOIN/aggregate = NOT
  auto-updatable; only the single-table `item_sizes` view auto-updates):
  - [x] **`inventory` unit-change + yield-change** UPDATEs of `quantity_used` (3 legacy recipe tables →
        one `recipe_lines` UPDATE). DONE (2 tests repointed). Delete-guard unchanged (reads work via views).
  - [ ] `delivery` branch/channel override writes — **SUPERSEDED** by `PUT /menu-price-overrides` (the merged
        Pricing endpoint). Dead in prod (their views aren't updatable); the dashboard uses the new endpoint.
        Retire at teardown (not blocking — no live caller writes them post-flip).
  - [ ] Retire the legacy catalog CRUD write endpoints in `menu`/`recipes` — **teardown-time** (dead in prod,
        Menu Studio supersedes them; their tests still pass on real tables so leaving them is harmless until
        teardown). `demo` seed writes are dev-only.
- **Reads stay as-is** — ✅ **PROVEN**: with the shim applied to a backfilled dump, `costing::service`'s SKU
  rollup returns **byte-identical** results (71 rows) reading the `item_sizes`/`menu_item_recipes` VIEWS vs
  the legacy tables. So `costing`, `orders` snapshot resolve, `menu_advisor`, `reports`, `delivery` catalog
  reads, `bundles` need NO change and NO fixture churn.
- **Deploy**: apply `deploy/menu_unification_shim.sql` after the backfill; regen OpenAPI.

The per-module grep map below still documents every legacy reference (useful for the retire/repoint audit),
but most **R** rows need NO change under this strategy — only the **W** rows above.

Legend: **R** = reads legacy table, **W** = writes, **owner** = CRUD owner. `menu_unification` (the
backfill) intentionally still reads legacy tables — leave it. `tests.rs` fixtures are excluded from the
counts below; update them alongside each module.

## Grep map (legacy table → modules, current `src/`)

| Legacy table | Modules referencing it | Wave-2 action |
|---|---|---|
| `menu_item_recipes` | recipes(owner), costing×2, bundles, orders×2, menu, menu_advisor, delivery, inventory, orgs, demo | → `recipe_lines` (owner_type='item_size') joined via `menu_item_sizes` |
| `addon_items` | menu(owner)×2, costing, orders×2, delivery×2, reports, tickets, recipes, orgs, permissions×2, demo×2, openapi | → `modifier_options` (legacy_source='addon') + `modifier_groups` |
| `addon_item_ingredients` | recipes(owner)×2, costing×2, orders×2, menu, delivery, inventory | → `recipe_lines` (owner_type='modifier_option') |
| `menu_item_addon_slots` | menu(owner) | → `menu_item_modifier_groups` (legacy_origin='slot') |
| `menu_item_optional_fields` | menu(owner), orders×2, delivery, inventory, recipes | → `modifier_options` (legacy_source='optional') + `recipe_lines` |
| `menu_item_allowed_addons` | menu(owner), delivery | → `menu_item_modifier_groups.included_option_ids` |
| `menu_item_addon_overrides` | menu (dead — 500s at runtime) | **delete** the list/upsert/delete endpoints; TEARDOWN drops the table |
| `branch_menu_overrides` | menu×2, orders, delivery×2, openapi | → `menu_price_overrides` (scope='branch', target menu_item_size) |
| `branch_menu_size_overrides` | menu, orders, delivery×2 | → `menu_price_overrides` (scope='branch') |
| `branch_addon_overrides` | menu×2, orders, delivery, openapi | → `menu_price_overrides` (target modifier_option) |
| `branch_channel_menu_overrides` | delivery×3 | → `menu_price_overrides` (scope='branch_channel') |
| `branch_channel_addon_overrides` | delivery×3 | → `menu_price_overrides` (scope='branch_channel', modifier_option) |
| `item_sizes` | menu, costing, orders, delivery×2, menu_advisor, demo×2, bin | → `menu_item_sizes` (`price_override`→`price`, add `sort`) |

## Per-module checklist

- [ ] **`src/recipes`** (owner) — rewrite CRUD onto `recipe_lines`. Keep `normalize_recipe_unit`
      (base-unit + yield via `org_ingredients.density_g_per_ml`/`yield_pct`) — it now writes
      `recipe_lines.quantity/unit`. `handlers.rs:93 ORDER BY size_label` = list ordering (repoint to
      `menu_item_sizes`). Recipe owner can be an item size **or** a modifier option.
- [ ] **`src/menu`** (owner, the big one) — build the Menu Studio endpoints (`CONTRACT §5.1`):
      item basics; `menu_item_sizes` CRUD; reusable `modifier_groups`/`modifier_options` CRUD;
      `menu_item_modifier_groups` attach-set (+ `included_option_ids`); item `Options`; deep
      `POST /menu-items/{id}/duplicate` (copies sizes+recipes+attachments+options+overrides — fixes the
      silent-drop bug); `GET /menu-items/{id}/cost` for inline fixes. **Delete** the dead
      `menu_item_addon_overrides` endpoints. Rename the "Drinks"-specific naming to generic.
      `handlers.rs:2441 ORDER BY size_label`.
- [ ] **`src/costing`** (canonical engine) — repoint `sku_costs_impl` + `org_addon_costs` to
      `recipe_lines` + `menu_item_sizes` + `modifier_options`. Preserve the NULL-tolerance contract
      (`FILTER (... IS NOT NULL)`, `bool_or(... IS NULL) → cost_incomplete`; never `COALESCE(...,0)`).
      Keep per-branch `branch_inventory.cost_per_unit` fallback. Update `costing/backfill.rs`'s inline
      rollup + the `bin` cost-snapshot repricer.
- [x] **`src/bundles`** — ✅ DONE (cost routed through `costing::service` via new `component_cost`; `computed_cost`/`item_cost` now nullable + `cost_missing`; recipe-less = unknown; `bundle-margin-flip` diff bin added & validated; 12 tests pass). Original scope: **RETIRE `compute_item_cost` (`handlers.rs:219`)**. Route bundle component
      cost through `costing::service`. Fix the NULL→0 bug at `handlers.rs:281` (`.unwrap_or(0)` → NULL-aware
      `computed_cost`/`cost_missing`). The 1.2× margin floor (`handlers.rs:383`, input built `:374`)
      keeps blocking on unknown cost, but now with the correct engine. `bundle_components` has no size
      dimension → cost each component at its `one_size`/default size (document the choice). Bundles show
      cost/profit **on create** (validate before insert). Produce the **margin-flip diff** (Wave 5).
- [ ] **`src/orders`** — repoint `component_resolve.rs` + `preview_recipe`/order-create snapshot to
      `recipe_lines` + `modifier_options` + `menu_price_overrides`. **Fix the arbitrary-size sites**
      `component_resolve.rs:132` and `handlers.rs:2459` (`ORDER BY size_label LIMIT 1`) — resolve the
      recipe by the order line's **actual** `size_label`. Wire legacy order-create translation
      (stable-id pass-through for `addon_item_id`/`optional_field_id`, `size_label`→`menu_item_sizes`).
      **Never** rewrite `order_items`/`order_item_addons`/`order_item_optionals` history.
- [ ] **`src/menu_advisor`** — repoint `adapter.rs` snapshot/point-in-time rollups to `recipe_lines` +
      `menu_item_sizes`. `ItemKey (menu_item_id, size_label)` unchanged.
- [ ] **`src/reports`** — repoint the addon report join (`addon_items` → `modifier_options` or the shim
      view). Current-basis menu-engineering auto-fixes once `costing` is repointed.
- [ ] **`src/delivery`** — repoint public catalog + `snapshot.rs` to the new tables + `menu_price_overrides`
      (all channel overrides). Milk/coffee swap now via `modifier_groups.legacy_addon_type` +
      `modifier_options` recipe (0-qty swap marker → deduct base recipe qty). Feeds the POS
      `GET /catalog/sync` (`CONTRACT §5.2`).
- [ ] **`src/inventory`** — repoint the unit-change `quantity_used` UPDATEs + the ingredient delete-guard
      (`handlers.rs:469/526/650` region) from the three legacy recipe tables to `recipe_lines`.
- [ ] **`src/orgs`** — repoint onboarding counts (items-with-recipes, addons) to new tables.
- [ ] **`src/tickets`** — repoint addon-name resolution to `modifier_options`/shim view.
- [ ] **`src/demo`** — repoint seed/sweeper to the new tables (or seed legacy + run backfill in demo setup).
- [ ] **`src/permissions`** — `addon_items`/`menu_items` are permission **resource-name strings**, not
      SQL. Keep the resource names (or alias); no DB change.
- [ ] **`src/bin`** — audit `backfill_cost_snapshots.rs` `item_sizes` ref; repoint to `menu_item_sizes`.
- [ ] **`src/openapi`** — regenerate after handlers change (`cargo run --bin export-openapi`).

## Catalog revision + sync

- [ ] Bump `catalog_revision.revision` (per org) on every catalog write (sizes, groups, options,
      attachments, recipes, overrides). Add `GET /catalog/sync` (`CONTRACT §5.2`).

## FLIP mechanics (one deploy, with the code above)

1. Deploy EXPAND migration (already merged) + run `backfill-menu-unification` per org (`--dry-run` first;
   review the report; the fleet dumps show 0 failures, only `info.implicit_all_addons`).
2. Deploy the repointed handlers (read/write **new** tables directly).
3. Apply `deploy/menu_unification_shim.sql` (drops legacy catalog/override tables, creates the compat
   VIEWS). Legacy external reads now hit views; legacy order-create writes translate old→new.
4. `cargo sqlx prepare` to refresh `.sqlx` (queries now reference new tables); `cargo run --bin export-openapi`.
5. Green build + `scripts/preflight.sh` (fmt + clippy + `cargo test --lib`). Watch for `#[sqlx::test]`
   DBs that rebuild from the full migration set — they need the `sufrix` role (CLAUDE.md) and now the
   new tables.

## Verify (Wave 5)

Backend + rust-core + dashboard + Flutter build/typecheck/test green; backfill dry-run clean; offline
pricing parity (old vs new) passes; bundle margin-flip diff produced; then write
`POS_MIGRATION_RUNBOOK.md` + `SHIM_TEARDOWN_PROMPT.md`.
