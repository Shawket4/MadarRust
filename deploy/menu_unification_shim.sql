-- ════════════════════════════════════════════════════════════════════════════
-- Menu / Recipe / Modifier unification — COMPAT SHIM (CONTRACT phase).
-- ════════════════════════════════════════════════════════════════════════════
-- APPLIED AT THE WAVE-2 SOURCE-OF-TRUTH FLIP — *not* during Wave 1, and *not* by
-- sqlx::migrate! (it lives outside migrations/ deliberately). Apply it in the same
-- deploy as the Wave-2 handler rewrite, AFTER the stable-id backfill has populated the
-- new tables for every org. It DROPS the legacy tables and recreates them as read-only
-- VIEWS that reproject the new unified tables into the exact old shapes, so the
-- currently-deployed Flutter build (old madar-core + madar-api hitting old endpoints)
-- keeps reading catalog data UNCHANGED.
--
-- WRITE PATH (handled in Rust, not here): deployed clients only WRITE order history
-- (order_item_addons / order_item_optionals / order_items.size_label) — real tables,
-- never dropped. Their addon_item_id / optional_field_id values equal the new
-- modifier_options.id (stable-id invariant), so legacy order-create keeps resolving via
-- a thin old→new translation in the order handlers. Catalog writes are dashboard-only
-- (Wave 3) and target the new tables directly. The hard FKs from order history into the
-- dropped catalog tables are replaced by soft references validated against the views.
--
-- Keep this shim until the new Flutter build is rolled out fleet-wide; then run
-- SHIM_TEARDOWN (removes these views + legacy endpoints + dead columns).
--
-- Money stays integer piastres. Unknown cost stays NULL.
-- ════════════════════════════════════════════════════════════════════════════

BEGIN;

-- ── Drop the legacy catalog/override tables (data now lives in the new tables). ──
-- CASCADE removes only the FK *constraints* from order history into these tables; the
-- order-history rows and their stable ids are untouched.
DROP TABLE IF EXISTS menu_item_addon_overrides        CASCADE;  -- dead table, no view
DROP TABLE IF EXISTS branch_channel_addon_overrides   CASCADE;
DROP TABLE IF EXISTS branch_channel_menu_overrides    CASCADE;
DROP TABLE IF EXISTS branch_addon_overrides           CASCADE;
DROP TABLE IF EXISTS branch_menu_size_overrides       CASCADE;
DROP TABLE IF EXISTS branch_menu_overrides            CASCADE;
DROP TABLE IF EXISTS menu_item_allowed_addons         CASCADE;
DROP TABLE IF EXISTS menu_item_optional_fields        CASCADE;
DROP TABLE IF EXISTS menu_item_addon_slots            CASCADE;
DROP TABLE IF EXISTS addon_item_ingredients           CASCADE;
DROP TABLE IF EXISTS addon_items                      CASCADE;
DROP TABLE IF EXISTS menu_item_recipes                CASCADE;
DROP TABLE IF EXISTS item_sizes                       CASCADE;

-- ── item_sizes ← menu_item_sizes (drop the synthetic one_size sentinel rows). ──
CREATE VIEW item_sizes AS
SELECT z.id, z.menu_item_id, z.label, z.price AS price_override, z.is_active
FROM menu_item_sizes z
WHERE NOT (z.label = 'one_size' AND z.id = (md5(z.menu_item_id::text || ':one_size'))::uuid);

-- ── menu_item_recipes ← recipe_lines (owner_type='item_size'). ──
-- size_label comes from the size dictionary; ingredient_name from org_ingredients
-- (id-keyed now, so renames no longer orphan). org_ingredient_id is always populated.
CREATE VIEW menu_item_recipes AS
SELECT rl.id, ms.menu_item_id, ms.label AS size_label, rl.quantity AS quantity_used,
       rl.created_at, rl.updated_at,
       oi.name AS ingredient_name, rl.unit AS ingredient_unit, rl.ingredient_id AS org_ingredient_id
FROM recipe_lines rl
JOIN menu_item_sizes ms ON ms.id = rl.owner_id
JOIN org_ingredients oi ON oi.id = rl.ingredient_id
WHERE rl.owner_type = 'item_size';

-- ── addon_items ← modifier_options (legacy_source='addon') + their group. ──
CREATE VIEW addon_items AS
SELECT mo.id, mg.org_id, mo.name, mg.legacy_addon_type AS type, mo.price AS default_price,
       mo.is_active, mo.name_translations, mo.created_at, mo.updated_at
FROM modifier_options mo
JOIN modifier_groups mg ON mg.id = mo.group_id
WHERE mo.legacy_source = 'addon';

-- ── addon_item_ingredients ← recipe_lines of addon options. ──
CREATE VIEW addon_item_ingredients AS
SELECT rl.id, rl.owner_id AS addon_item_id, rl.quantity AS quantity_used, rl.created_at, rl.updated_at,
       oi.name AS ingredient_name, rl.unit AS ingredient_unit, rl.ingredient_id AS org_ingredient_id
FROM recipe_lines rl
JOIN modifier_options mo ON mo.id = rl.owner_id AND mo.legacy_source = 'addon'
JOIN org_ingredients oi ON oi.id = rl.ingredient_id
WHERE rl.owner_type = 'modifier_option';

-- ── menu_item_addon_slots ← attachments whose provenance is a slot. ──
-- (Allowlist-only attachments are NOT slots — they surface via menu_item_allowed_addons.)
CREATE VIEW menu_item_addon_slots AS
SELECT mimg.id, mimg.menu_item_id, mg.legacy_addon_type AS addon_type,
       COALESCE(mimg.is_required_override, mg.is_required)   AS is_required,
       COALESCE(mimg.min_override, mg.min_selections)        AS min_selections,
       COALESCE(mimg.max_override, mg.max_selections)        AS max_selections,
       now() AS created_at,          -- slot.created_at not preserved (attach is synthesized)
       mg.name AS label, mg.name_translations AS label_translations
FROM menu_item_modifier_groups mimg
JOIN modifier_groups mg ON mg.id = mimg.group_id
WHERE mimg.legacy_origin = 'slot' AND mg.legacy_addon_type IS NOT NULL;

-- ── menu_item_optional_fields ← modifier_options (legacy_source='optional'). ──
-- menu_item_id via the item's per-item Options group attachment; inline recipe via the
-- option's (≤1) recipe_line. size_label scoping is not representable → NULL (reported by
-- the backfill as optional.size_scoped).
CREATE VIEW menu_item_optional_fields AS
SELECT mo.id, mimg.menu_item_id, mo.name, mo.price,
       rl.ingredient_id AS org_ingredient_id, oi.name AS ingredient_name,
       rl.unit AS ingredient_unit, rl.quantity AS quantity_used,
       NULL::text AS size_label, mo.is_active, mo.created_at, mo.updated_at, mo.name_translations
FROM modifier_options mo
JOIN menu_item_modifier_groups mimg ON mimg.group_id = mo.group_id AND mimg.legacy_origin = 'options'
LEFT JOIN recipe_lines rl ON rl.owner_type = 'modifier_option' AND rl.owner_id = mo.id
LEFT JOIN org_ingredients oi ON oi.id = rl.ingredient_id
WHERE mo.legacy_source = 'optional';

-- ── menu_item_allowed_addons ← unnest included_option_ids (any attach). ──
-- included_option_ids NULL = no allowlist (item offered all org addons) = no rows here,
-- faithfully reproducing the legacy "no rows = org catalog default" semantics.
CREATE VIEW menu_item_allowed_addons AS
SELECT mimg.menu_item_id, u.opt AS addon_item_id, (u.ord - 1)::int AS sort_order, now() AS created_at
FROM menu_item_modifier_groups mimg
CROSS JOIN LATERAL unnest(mimg.included_option_ids) WITH ORDINALITY AS u(opt, ord)
JOIN modifier_options mo ON mo.id = u.opt AND mo.legacy_source = 'addon'
WHERE mimg.included_option_ids IS NOT NULL;

-- ── branch_menu_overrides ← per-(branch,item) rollup of size-scoped overrides. ──
-- price = the one_size override (size-less items); is_available = false iff any of the
-- item's sizes is explicitly unavailable at the branch, else true.
CREATE VIEW branch_menu_overrides AS
SELECT p.branch_id, ms.menu_item_id,
       max(p.price) FILTER (WHERE ms.label = 'one_size') AS price_override,
       COALESCE(bool_and(p.is_available), true) AS is_available,
       max(p.updated_at) AS updated_at
FROM menu_price_overrides p
JOIN menu_item_sizes ms ON ms.id = p.target_id
WHERE p.scope = 'branch' AND p.target_type = 'menu_item_size'
GROUP BY p.branch_id, ms.menu_item_id
-- suppress no-op rows (an item that only has per-size price overrides, no item-level signal)
HAVING max(p.price) FILTER (WHERE ms.label = 'one_size') IS NOT NULL
    OR bool_or(p.is_available = false);

-- ── branch_menu_size_overrides ← per-size price overrides (non one_size). ──
CREATE VIEW branch_menu_size_overrides AS
SELECT p.branch_id, ms.menu_item_id, ms.label AS size_label, p.price AS price_override, p.updated_at
FROM menu_price_overrides p
JOIN menu_item_sizes ms ON ms.id = p.target_id
WHERE p.scope = 'branch' AND p.target_type = 'menu_item_size'
  AND p.price IS NOT NULL AND ms.label <> 'one_size';

-- ── branch_addon_overrides ← modifier_option branch overrides. ──
CREATE VIEW branch_addon_overrides AS
SELECT p.branch_id, p.target_id AS addon_item_id, p.price AS price_override,
       COALESCE(p.is_available, true) AS is_available, p.updated_at
FROM menu_price_overrides p
JOIN modifier_options mo ON mo.id = p.target_id AND mo.legacy_source = 'addon'
WHERE p.scope = 'branch' AND p.target_type = 'modifier_option';

-- ── branch_channel_menu_overrides ← per-(branch,item,channel) rollup. ──
CREATE VIEW branch_channel_menu_overrides AS
SELECT p.branch_id, ms.menu_item_id, p.channel,
       max(p.price) FILTER (WHERE ms.label = 'one_size') AS price_override,
       bool_and(p.is_available) AS is_available,     -- nullable tri-state, as before
       max(p.updated_at) AS updated_at
FROM menu_price_overrides p
JOIN menu_item_sizes ms ON ms.id = p.target_id
WHERE p.scope = 'branch_channel' AND p.target_type = 'menu_item_size'
GROUP BY p.branch_id, ms.menu_item_id, p.channel
HAVING max(p.price) FILTER (WHERE ms.label = 'one_size') IS NOT NULL
    OR bool_or(p.is_available IS NOT NULL);

-- ── branch_channel_addon_overrides ← modifier_option channel overrides. ──
CREATE VIEW branch_channel_addon_overrides AS
SELECT p.branch_id, p.target_id AS addon_item_id, p.channel, p.price AS price_override, p.is_available, p.updated_at
FROM menu_price_overrides p
JOIN modifier_options mo ON mo.id = p.target_id AND mo.legacy_source = 'addon'
WHERE p.scope = 'branch_channel' AND p.target_type = 'modifier_option';

-- ── Grants (match repo convention; the legacy 'sufrix' role must exist). ──
GRANT SELECT ON item_sizes, menu_item_recipes, addon_items, addon_item_ingredients,
                menu_item_addon_slots, menu_item_optional_fields, menu_item_allowed_addons,
                branch_menu_overrides, branch_menu_size_overrides, branch_addon_overrides,
                branch_channel_menu_overrides, branch_channel_addon_overrides TO sufrix;

COMMIT;
