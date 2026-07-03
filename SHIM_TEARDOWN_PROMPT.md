# SHIM_TEARDOWN — ready-to-paste prompt

Paste this as a new task **only after** the entire Flutter POS fleet is confirmed on the new build
(every device reporting a `catalog_revision`-aware sync; see POS_MIGRATION_RUNBOOK.md). Until then the
compat shim MUST stay — deployed old clients read catalog data through it.

---

You are removing the now-unused backward-compat shim from the Madar menu/recipe/modifier unification.
The new unified tables have been the source of truth since the Wave-2 flip; `deploy/menu_unification_shim.sql`
recreated the legacy tables as read-only compat VIEWS for old Flutter clients. The fleet is now fully on
the new build, so the shim + legacy endpoints + dead columns can go. Work in `~/Desktop/MadarRust`.

**Preconditions to verify FIRST (abort if any fails):**
1. No client has requested a legacy catalog endpoint (see below) in the last N days — check access logs.
2. Every active org's devices sync via `GET /catalog/sync` with a `catalog_revision` — no legacy sync callers.
3. A fresh DB backup exists (`db-backups/`).

**Do, in this order (one migration + code):**

1. **Drop the compat VIEWS** (they are `deploy/menu_unification_shim.sql`'s outputs — now unused):
   `item_sizes`, `menu_item_recipes`, `addon_items`, `addon_item_ingredients`, `menu_item_addon_slots`,
   `menu_item_optional_fields`, `menu_item_allowed_addons`, `branch_menu_overrides`,
   `branch_menu_size_overrides`, `branch_addon_overrides`, `branch_channel_menu_overrides`,
   `branch_channel_addon_overrides`. Write a migration `..._menu_unification_teardown.sql` with
   `DROP VIEW IF EXISTS <each>;`. Also `DROP TABLE IF EXISTS menu_item_addon_overrides;` (the reconstructed
   dead table — never used).

2. **Remove the legacy REST endpoints + their handlers/routes/OpenAPI** that only existed to serve old
   clients the old shapes. Grep `src/` for the old paths and delete the handlers that read the views:
   the standalone `/addon-items` CRUD, `/menu-items/{id}/addon-slots`, `/menu-items/{id}/optional-fields`
   (old shape), `/menu-items/{id}/allowed-addons`, `/menu-items/{id}/overrides` (already dead), and the
   legacy branch/branch-channel override endpoints — keep ONLY the unified Menu Studio endpoints
   (`CONTRACT.md §5.1`) and `menu-price-overrides`. Remove the old→new write-translation shim in the order
   handlers **only if** no old client posts orders (else keep the order-create translation — it is cheap
   and stable-id based). Regenerate the OpenAPI spec (`cargo run --bin export-openapi`).

3. **Drop the dead compat columns** once nothing reads them:
   - `modifier_options.legacy_source` and `modifier_groups.legacy_addon_type` — **CAUTION**: keep
     `legacy_addon_type` if milk/coffee ingredient-swap resolution still keys on it (grep
     `'milk_type'`/`'coffee_type'` in `orders`/`delivery`; if the swap was re-modeled onto
     `modifier_options.replaces_ingredient_id`, then it can go, else KEEP).
   - `menu_item_modifier_groups.legacy_origin` — safe to drop (only the shim views read it).

4. **Remove the retired cost engine + one-shot tooling:**
   - Delete `bundles::compute_item_cost` (`src/bundles/handlers.rs`, doc-marked "RETIRED") and the
     `bundle-margin-flip` bin (`src/bin/bundle_margin_flip.rs` + its `[[bin]]` in `Cargo.toml`) — both
     exist only for the migration diff.
   - Optionally retire the `backfill-menu-unification` bin once no org will ever be re-backfilled.

5. **Verify:** `cargo build`; `scripts/preflight.sh` (fmt + clippy + `cargo test --lib`) green; re-run the
   dashboard + rust-core + Flutter builds; confirm no reference to any dropped view/column/endpoint remains
   (`grep -r` the names). Update `CONTRACT.md` to mark the shim retired.

Do NOT touch order history (`order_items`, `order_item_addons`, `order_item_optionals`,
`order_line_bundle_components`, any `*_cost`/`size_label`) — it stays immutable forever, resolving via the
stable `modifier_options.id` / `menu_item_sizes` ids.
